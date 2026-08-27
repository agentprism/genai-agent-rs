//! Cloneable Models registry and request router from Architecture v2 part 1
//! §3.6 and part 2 §2.6.

use crate::{
    AiError, ApiFamily, ApiRequestOptions, AssistantMessage, AttemptMiddleware, AuthContext,
    AuthInteraction, AuthResolutionOverrides, AuthResolutionPurpose, CacheRetention,
    CancellationToken, CatalogError, CatalogFetchContext, CatalogSnapshot, Context, Credential,
    CredentialInfo, CredentialStore, DeferredCancelOptions, DeferredFetchOptions, DeferredHandle,
    DeferredModelRuntime, EmptyAuthContext, ErasedApiFullOptions, ErasedPayloadTransform,
    HeaderTransform, HeaderTransformContext, ImageHeaderTransformContext, InMemoryCredentialStore,
    InMemoryModelOverrideStore, InMemoryModelsStore, LocalAssistantStream, LocalAttemptMiddleware,
    LocalAuthContext, LocalAuthInteraction, LocalBoxFuture, LocalCredentialStore,
    LocalDeferredModelRuntime, LocalErasedPayloadTransform, LocalHeaderTransform,
    LocalInMemoryCredentialStore, LocalInMemoryModelOverrideStore, LocalInMemoryModelsStore,
    LocalModelOverrideStore, LocalModelRuntime, LocalModelsStore, LocalPayloadTransform,
    LocalPayloadTransformAdapter, LocalProviderCatalogState, LocalProviderRefreshCoordination,
    LocalProviderRegistration, LocalResolvedApiRequest, LocalResolvedDeferredRequest,
    LocalResponseObserver, ModelDescriptor, ModelOverride, ModelOverrideStore, ModelRef,
    ModelRequest, ModelRuntime, ModelsStore, PayloadTransform, PayloadTransformAdapter,
    ProviderCatalogLayers, ProviderCatalogState, ProviderRefreshCoordination,
    ProviderRefreshResult, ProviderRegistration, ProviderRegistrationError, RefreshGeneration,
    RefreshReport, RefreshRequest, RequestStartError, RequestStartErrorKind, ResolvedApiRequest,
    ResolvedDeferredRequest, ResponseObserver, SendBoxFuture, SimpleGenerationOptions,
    apply_anthropic_messages_default_headers, apply_header_spec,
    apply_openai_codex_responses_session_affinity_headers,
    apply_openai_completions_session_affinity_headers,
    apply_openai_responses_session_affinity_headers, local_provider_default_headers,
    merge_header_map, provider_default_headers, publish_candidate, publish_local_candidate,
    restore_local_persisted_candidate, restore_persisted_candidate,
};
use futures_util::future::{Either, FutureExt, Shared, join_all, select};
use futures_util::stream::{FuturesUnordered, StreamExt};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::rc::Rc;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

/// Immutable snapshot of currently registered providers in registration order.
pub type ProviderSnapshot = Arc<[Arc<ProviderRegistration>]>;

/// Immutable flattened model snapshot in provider registration order.
pub type ModelSnapshot = Arc<[ModelDescriptor]>;

/// Immutable flattened image-model snapshot in provider registration order.
pub type ImageModelSnapshot = Arc<[crate::ImageModelDescriptor]>;

type ImageRefreshFuture = Shared<
    futures_util::future::BoxFuture<'static, Result<ImageModelSnapshot, crate::ImageCatalogError>>,
>;

type ImageRefreshKey = (crate::ProviderId, u64);

struct ImageRefreshTask {
    future: ImageRefreshFuture,
}

/// Concrete cloneable model/provider/auth control-plane handle.
#[derive(Clone)]
pub struct Models {
    inner: Arc<ModelsInner>,
}

struct ModelsInner {
    providers: RwLock<IndexMap<crate::ProviderId, ProviderSlot>>,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
    models_store: Arc<dyn ModelsStore>,
    override_store: Arc<dyn ModelOverrideStore>,
    header_transforms: Arc<[Arc<dyn HeaderTransform>]>,
    payload_transforms: Arc<[Arc<dyn ErasedPayloadTransform>]>,
    response_observers: Arc<[Arc<dyn ResponseObserver>]>,
    attempt_middleware: Arc<[Arc<dyn AttemptMiddleware>]>,
    image_refreshes: std::sync::Mutex<BTreeMap<ImageRefreshKey, Arc<ImageRefreshTask>>>,
}

struct ProviderSlot {
    // Coordination is intentionally retained when registration is absent.
    // Pinned pi keeps its generation and publication-chain maps across
    // delete/re-add, so a stale durable write cannot escape serialization
    // merely because the provider was temporarily removed.
    registration: Option<Arc<ProviderRegistration>>,
    coordination: Arc<ProviderRefreshCoordination>,
    image_registration_generation: u64,
}

#[derive(Clone, Copy)]
enum DeferredOperation {
    Fetch,
    Cancel,
}

struct DeferredResolutionRequest {
    model_ref: ModelRef,
    handle: DeferredHandle,
    wait_ms: Option<u64>,
    request_options: ApiRequestOptions,
    auth_overrides: AuthResolutionOverrides,
    operation: DeferredOperation,
    cancellation: CancellationToken,
}

impl fmt::Debug for Models {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Models")
            .field("provider_count", &self.providers().len())
            .finish_non_exhaustive()
    }
}

impl Default for Models {
    fn default() -> Self {
        Self::builder()
            .build()
            .expect("empty Models registration is valid")
    }
}

impl Models {
    /// Starts an immutable Models configuration builder.
    pub fn builder() -> ModelsBuilder {
        ModelsBuilder::default()
    }

    /// Returns the current provider snapshot without retaining the registry
    /// read lock.
    pub fn providers(&self) -> ProviderSnapshot {
        let providers = read_unpoisoned(&self.inner.providers)
            .values()
            .filter_map(|slot| slot.registration.as_ref().map(Arc::clone))
            .collect::<Vec<_>>();
        Arc::from(providers)
    }

    /// Returns one registered provider handle without retaining a registry
    /// read lock.
    pub fn provider(&self, provider: &crate::ProviderId) -> Option<Arc<ProviderRegistration>> {
        read_unpoisoned(&self.inner.providers)
            .get(provider)
            .and_then(|slot| slot.registration.as_ref().map(Arc::clone))
    }

    /// Returns a flattened snapshot of current provider catalog snapshots.
    pub fn models(&self) -> ModelSnapshot {
        let providers = self.providers();
        let models = providers
            .iter()
            .flat_map(|provider| {
                provider
                    .catalog
                    .snapshot()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        Arc::from(models)
    }

    /// Returns the distinct image-generation catalog in provider order.
    pub fn image_models(&self) -> ImageModelSnapshot {
        Arc::from(
            self.providers()
                .iter()
                .flat_map(|provider| provider.image_models.iter().cloned())
                .collect::<Vec<_>>(),
        )
    }

    /// Returns one provider's current image-model snapshot synchronously.
    ///
    /// Like Pi's `ImagesModels.getModels(provider)`, this never initiates a
    /// refresh. Unknown providers produce an empty snapshot.
    pub fn image_models_for(&self, provider_id: &crate::ProviderId) -> ImageModelSnapshot {
        self.provider(provider_id).map_or_else(
            || Arc::from(Vec::new()),
            |provider| Arc::clone(&provider.image_models),
        )
    }

    /// Looks up one image-generation model synchronously.
    pub fn image_model(&self, model_ref: &ModelRef) -> Option<crate::ImageModelDescriptor> {
        self.provider(&model_ref.provider)?
            .image_models
            .iter()
            .find(|model| model.model_ref == *model_ref)
            .cloned()
    }

    /// Explicitly refreshes dynamic image-model providers.
    ///
    /// A selected provider's failure is returned. Refreshing all providers is
    /// best-effort and never fails because one provider fails. Static and
    /// unknown providers are no-ops, matching Pi's `ImagesModels.refresh`.
    pub fn refresh_image_models(
        &self,
        provider_id: Option<crate::ProviderId>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), crate::ImageCatalogError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return if provider_id.is_some() {
                    Err(crate::ImageCatalogError::new(
                        "image catalog refresh cancelled",
                    ))
                } else {
                    Ok(())
                };
            }
            if let Some(provider_id) = provider_id {
                let Some((provider, generation)) =
                    self.image_provider_with_generation(&provider_id)
                else {
                    return Ok(());
                };
                self.refresh_image_provider(provider, generation, cancellation)
                    .await?;
                return Ok(());
            }
            let _ = join_all(self.image_providers_with_generations().into_iter().map(
                |(provider, generation)| {
                    let cancellation = cancellation.clone();
                    async move {
                        self.refresh_image_provider(provider, generation, cancellation)
                            .await
                    }
                },
            ))
            .await;
            Ok(())
        })
    }

    async fn refresh_image_provider(
        &self,
        provider: Arc<ProviderRegistration>,
        registration_generation: u64,
        cancellation: CancellationToken,
    ) -> Result<ImageModelSnapshot, crate::ImageCatalogError> {
        let Some(source) = provider.image_model_source.as_ref().map(Arc::clone) else {
            return Ok(Arc::clone(&provider.image_models));
        };
        let provider_id = provider.descriptor.id.clone();
        let key = (provider_id.clone(), registration_generation);
        let refresh = {
            let mut refreshes = self
                .inner
                .image_refreshes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(refreshes.entry(key.clone()).or_insert_with(|| {
                let source = Arc::clone(&source);
                Arc::new(ImageRefreshTask {
                    future:
                        async move { source.fetch(CancellationToken::new()).await.map(Arc::from) }
                            .boxed()
                            .shared(),
                })
            }))
        };
        let result = match select(
            Box::pin(refresh.future.clone()),
            Box::pin(cancellation.cancelled()),
        )
        .await
        {
            Either::Left((result, _)) => result,
            Either::Right(((), _)) => {
                return Err(crate::ImageCatalogError::new(
                    "image catalog refresh cancelled",
                ));
            }
        };
        let mut refreshes = self
            .inner
            .image_refreshes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if refreshes
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, &refresh))
        {
            refreshes.remove(&key);
        }
        drop(refreshes);
        let snapshot = result?;
        self.publish_image_snapshot_if_current(
            &provider,
            registration_generation,
            Arc::clone(&snapshot),
        )?;
        Ok(snapshot)
    }

    fn image_provider_with_generation(
        &self,
        provider: &crate::ProviderId,
    ) -> Option<(Arc<ProviderRegistration>, u64)> {
        let providers = read_unpoisoned(&self.inner.providers);
        let slot = providers.get(provider)?;
        Some((
            Arc::clone(slot.registration.as_ref()?),
            slot.image_registration_generation,
        ))
    }

    fn image_providers_with_generations(&self) -> Vec<(Arc<ProviderRegistration>, u64)> {
        read_unpoisoned(&self.inner.providers)
            .values()
            .filter_map(|slot| {
                slot.registration
                    .as_ref()
                    .map(|provider| (Arc::clone(provider), slot.image_registration_generation))
            })
            .collect()
    }

    fn publish_image_snapshot_if_current(
        &self,
        provider: &Arc<ProviderRegistration>,
        registration_generation: u64,
        snapshot: ImageModelSnapshot,
    ) -> Result<(), crate::ImageCatalogError> {
        let mut replacement = (**provider).clone();
        replacement.image_models = snapshot;
        replacement.validate().map_err(|error| {
            crate::ImageCatalogError::new(format!("invalid image catalog: {error}"))
        })?;
        let replacement = Arc::new(replacement);
        let mut providers = write_unpoisoned(&self.inner.providers);
        let Some(slot) = providers.get_mut(&provider.descriptor.id) else {
            return Ok(());
        };
        if slot.image_registration_generation != registration_generation
            || !slot
                .registration
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, provider))
        {
            return Ok(());
        }
        slot.registration = Some(replacement);
        slot.image_registration_generation = slot.image_registration_generation.saturating_add(1);
        Ok(())
    }

    /// Generates images through the registered provider capability.
    ///
    /// Like Pi's `ImagesModels.generate`, provider and configuration failures
    /// are returned in-band as [`crate::AssistantImages`].
    pub fn generate_images(
        &self,
        model: crate::ImageModelDescriptor,
        context: crate::ImageGenerationContext,
        options: crate::ImageGenerationOptions,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, crate::AssistantImages> {
        Box::pin(async move {
            let failure_model = model.model_ref.clone();
            let failure_api = model.api.clone();
            let failure = |reason, message: String| {
                crate::AssistantImages::failure(
                    &failure_model,
                    failure_api.clone(),
                    reason,
                    message,
                )
            };
            if cancellation.is_cancelled() {
                return failure(
                    crate::ImageGenerationStopReason::Error,
                    "Request aborted".into(),
                );
            }
            let Some(provider) = self.provider(&model.model_ref.provider) else {
                return failure(
                    crate::ImageGenerationStopReason::Error,
                    format!("Unknown provider: {}", model.model_ref.provider),
                );
            };
            let Some(api) = provider.image_apis.get(&model.api).cloned() else {
                return failure(
                    crate::ImageGenerationStopReason::Error,
                    format!(
                        "provider {} has no image API for {}",
                        provider.descriptor.id, model.api
                    ),
                );
            };

            let crate::ImageGenerationOptions {
                request: request_options,
                auth: auth_overrides,
                metadata,
            } = options;
            let request_environment = auth_overrides.environment.clone();

            let auth = match await_or_cancelled(
                provider.auth.resolve(
                    crate::ResolveAuthRequest {
                        provider: provider.descriptor.clone(),
                        model: None,
                        purpose: AuthResolutionPurpose::Request,
                        credential_store: Arc::clone(&self.inner.credentials),
                        auth_context: Arc::clone(&self.inner.auth_context),
                        overrides: auth_overrides,
                    },
                    cancellation.clone(),
                ),
                &cancellation,
            )
            .await
            {
                Ok(auth) => auth,
                Err(_) => {
                    return failure(
                        crate::ImageGenerationStopReason::Error,
                        "Request aborted".into(),
                    );
                }
            };
            let auth = match auth {
                Ok(Some(auth)) => auth,
                Ok(None) => crate::ResolvedAuth {
                    api_key: None,
                    headers: http::HeaderMap::new(),
                    transport_headers: http::HeaderMap::new(),
                    environment: std::collections::BTreeMap::new(),
                    base_url: None,
                    source: crate::AuthSource::new("unconfigured"),
                },
                Err(error) => {
                    return failure(crate::ImageGenerationStopReason::Error, error.to_string());
                }
            };
            if cancellation.is_cancelled() {
                return failure(
                    crate::ImageGenerationStopReason::Error,
                    "Request aborted".into(),
                );
            }

            let endpoint = auth
                .base_url
                .clone()
                .or_else(|| provider.descriptor.base_url.clone())
                .unwrap_or_else(|| model.base_url.clone());
            let mut auth_headers = auth.transport_headers.clone();
            merge_header_map(&mut auth_headers, &auth.headers);
            let mut headers = match provider_default_headers(provider.as_ref()) {
                Ok(headers) => headers,
                Err(error) => {
                    return failure(crate::ImageGenerationStopReason::Error, error.to_string());
                }
            };
            if let Err(error) = apply_header_spec(&mut headers, &model.headers) {
                return failure(crate::ImageGenerationStopReason::Error, error.to_string());
            }
            merge_header_map(&mut headers, &auth.headers);
            if let Err(error) = apply_header_spec(&mut headers, &request_options.headers) {
                return failure(crate::ImageGenerationStopReason::Error, error.to_string());
            }
            for transform in self.inner.header_transforms.iter() {
                match await_or_cancelled(
                    transform.transform_image(
                        ImageHeaderTransformContext {
                            provider: &provider.descriptor.id,
                            model: &model,
                            api: &model.api,
                            endpoint: &endpoint,
                        },
                        &mut headers,
                    ),
                    &cancellation,
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        return failure(crate::ImageGenerationStopReason::Error, error.to_string());
                    }
                    Err(_) => {
                        return failure(
                            crate::ImageGenerationStopReason::Error,
                            "Request aborted".into(),
                        );
                    }
                }
            }
            let mut environment = auth.environment;
            environment.extend(request_environment);
            let mut retry_policy = provider.retry_policy.clone();
            if let Some(max_retries) = request_options.max_retries {
                retry_policy.max_retries = max_retries;
            }
            if let Some(max_delay_ms) = request_options.max_retry_delay_ms {
                retry_policy.max_server_delay = Some(Duration::from_millis(max_delay_ms));
            }
            let timeout = request_options.timeout_ms.map(Duration::from_millis);
            api.generate(
                crate::ResolvedImageRequest {
                    model,
                    context,
                    request_options,
                    endpoint,
                    headers,
                    auth_headers,
                    environment,
                    metadata,
                    api_key: auth.api_key,
                    payload_transforms: Arc::clone(&self.inner.payload_transforms),
                    retry_policy,
                    timeout,
                    retry_classifier: Arc::clone(&provider.retry_classifier),
                    response_observers: Arc::clone(&self.inner.response_observers),
                    attempt_middleware: Arc::clone(&self.inner.attempt_middleware),
                },
                cancellation,
            )
            .await
        })
    }

    /// Applies one provider's credential-scoped availability policy without
    /// changing its complete synchronous catalog.
    pub fn filter_models(
        &self,
        provider_id: &crate::ProviderId,
        credential: Option<&Credential>,
    ) -> ModelSnapshot {
        let Some(provider) = self.provider(provider_id) else {
            return Arc::from(Vec::new());
        };
        let models = provider.catalog.snapshot();
        Arc::from(provider.filter_models.as_ref().map_or_else(
            || models.iter().cloned().collect(),
            |filter| filter(models.as_ref(), credential),
        ))
    }

    /// Checks whether one provider has complete authentication configuration
    /// without refreshing OAuth or issuing a provider request.
    pub fn check_auth(
        &self,
        provider_id: crate::ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<crate::AuthCheck>, crate::AuthError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            let Some(provider) = self.provider(&provider_id) else {
                return Ok(None);
            };
            let credential = self
                .inner
                .credentials
                .read(provider_id, cancellation.clone())
                .await
                .map_err(crate::AuthError::from)?;
            check_provider_auth(
                provider.as_ref(),
                credential,
                Arc::clone(&self.inner.auth_context),
                cancellation,
            )
            .await
        })
    }

    /// Returns models whose providers have complete auth configuration,
    /// narrowed by any credential-scoped provider policy.
    pub fn get_available(
        &self,
        provider_id: Option<crate::ProviderId>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ModelSnapshot, crate::AuthError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            let providers = provider_id.map_or_else(
                || self.providers().iter().cloned().collect::<Vec<_>>(),
                |id| self.provider(&id).into_iter().collect(),
            );
            let checked = join_all(providers.iter().map(|provider| {
                let cancellation = cancellation.clone();
                async move {
                    let credential = self
                        .inner
                        .credentials
                        .read(provider.descriptor.id.clone(), cancellation.clone())
                        .await
                        .map_err(crate::AuthError::from)?;
                    let auth = check_provider_auth(
                        provider.as_ref(),
                        credential.clone(),
                        Arc::clone(&self.inner.auth_context),
                        cancellation,
                    )
                    .await?;
                    Ok::<_, crate::AuthError>((provider, credential, auth))
                }
            }))
            .await;

            let mut available = Vec::new();
            for result in checked {
                let (provider, credential, auth) = result?;
                if auth.is_none() {
                    continue;
                }
                let models = provider.catalog.snapshot();
                available.extend(provider.filter_models.as_ref().map_or_else(
                    || models.iter().cloned().collect(),
                    |filter| filter(models.as_ref(), credential.as_ref()),
                ));
            }
            Ok(Arc::from(available))
        })
    }

    /// Resolves a model synchronously against the latest provider snapshot.
    pub fn model(&self, model_ref: &ModelRef) -> Option<ModelDescriptor> {
        let provider = self.provider(&model_ref.provider)?;
        provider
            .catalog
            .snapshot()
            .iter()
            .find(|model| model.common.model_ref == *model_ref)
            .cloned()
    }

    /// Returns the Models-owned credential-store capability.
    pub fn credential_store(&self) -> Arc<dyn CredentialStore> {
        Arc::clone(&self.inner.credentials)
    }

    /// Lists stored credential metadata without resolving or exposing secrets.
    pub fn credential_info(
        &self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Vec<CredentialInfo>, crate::AuthError>> {
        Box::pin(async move {
            self.inner
                .credentials
                .list(cancellation)
                .await
                .map_err(crate::AuthError::from)
        })
    }

    /// Resolves provider-scoped authentication using explicit, stored, then
    /// ambient precedence. Unknown providers resolve to `None`, matching Pi's
    /// `Models.getAuth` behavior.
    pub fn resolve_auth(
        &self,
        provider_id: crate::ProviderId,
        overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<crate::ResolvedAuth>, crate::AuthError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            let Some(provider) = self.provider(&provider_id) else {
                return Ok(None);
            };
            await_auth_or_cancelled(
                provider.auth.resolve(
                    crate::ResolveAuthRequest {
                        provider: provider.descriptor.clone(),
                        model: None,
                        purpose: AuthResolutionPurpose::Request,
                        credential_store: Arc::clone(&self.inner.credentials),
                        auth_context: Arc::clone(&self.inner.auth_context),
                        overrides,
                    },
                    cancellation.clone(),
                ),
                &cancellation,
            )
            .await
        })
    }

    /// Runs provider-owned login and persists the result under a credential
    /// lease, so it serializes with refresh and concurrent login.
    pub fn login(
        &self,
        provider_id: crate::ProviderId,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Credential, crate::AuthError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            let provider = self.provider(&provider_id).ok_or_else(|| {
                crate::AuthError::new(
                    "unknown_provider",
                    format!("unknown provider: {provider_id}"),
                )
            })?;
            let credential = await_auth_or_cancelled(
                provider.auth.login(interaction, cancellation.clone()),
                &cancellation,
            )
            .await?;
            let mut lease = self
                .inner
                .credentials
                .acquire_lease(provider_id, cancellation.clone())
                .await?;
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            lease.replace(Some(credential.clone()));
            lease.commit().await?;
            Ok(credential)
        })
    }

    /// Deletes the stored credential under a provider-scoped lease. Matching
    /// pinned Pi, provider cleanup is not invoked by this Models operation.
    pub fn logout(
        &self,
        provider_id: crate::ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), crate::AuthError>> {
        Box::pin(async move {
            let mut lease = self
                .inner
                .credentials
                .acquire_lease(provider_id, cancellation.clone())
                .await?;
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            lease.replace(None);
            lease.commit().await.map_err(crate::AuthError::from)
        })
    }

    /// Atomically loads one provider's complete effective catalog snapshot.
    pub fn catalog_snapshot(&self, provider: &crate::ProviderId) -> Option<Arc<CatalogSnapshot>> {
        let registration = self.provider(provider)?;
        Some(
            registration
                .catalog
                .catalog_state()
                .map(|state| state.published_snapshot())
                .unwrap_or_else(|| {
                    Arc::new(CatalogSnapshot::baseline(
                        registration
                            .catalog
                            .snapshot()
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>(),
                    ))
                }),
        )
    }

    /// Returns one provider's current provenance layers.
    pub fn catalog_layers(&self, provider: &crate::ProviderId) -> Option<ProviderCatalogLayers> {
        self.provider(provider)?
            .catalog
            .catalog_state()
            .map(|state| state.layers())
    }

    /// Atomically validates and upserts a complete provider registration.
    pub fn set_provider(
        &self,
        provider: ProviderRegistration,
    ) -> Result<Option<Arc<ProviderRegistration>>, ProviderRegistrationError> {
        if let Some(state) = provider.catalog.catalog_state() {
            let host_overrides = self
                .inner
                .override_store
                .snapshot(&provider.descriptor.id)
                .map_err(|error| ProviderRegistrationError::Catalog {
                    provider: provider.descriptor.id.clone(),
                    message: error.message,
                })?;
            state
                .replace_host_overrides(host_overrides)
                .map_err(|error| ProviderRegistrationError::Catalog {
                    provider: provider.descriptor.id.clone(),
                    message: error.message,
                })?;
        }
        provider.validate()?;
        let provider = Arc::new(provider);
        let provider_id = provider.descriptor.id.clone();
        let mut providers = write_unpoisoned(&self.inner.providers);
        if let Some(slot) = providers.get_mut(&provider_id) {
            // Pi supersedes by provider ID before exposing the replacement.
            slot.coordination.supersede_refresh();
            if let Some(state) = provider.catalog.catalog_state() {
                state.bind_coordination(Arc::clone(&slot.coordination));
            }
            let previous = slot.registration.replace(provider);
            slot.image_registration_generation =
                slot.image_registration_generation.saturating_add(1);
            if previous.is_none() {
                // Map deletion followed by re-addition appends the provider to
                // registration order while retaining its separate coordinator.
                let slot = providers
                    .shift_remove(&provider_id)
                    .expect("provider slot was just observed");
                providers.insert(provider_id, slot);
            }
            return Ok(previous);
        }

        let coordination = Arc::new(ProviderRefreshCoordination::new());
        if let Some(state) = provider.catalog.catalog_state() {
            state.bind_coordination(Arc::clone(&coordination));
        }
        providers.insert(
            provider_id,
            ProviderSlot {
                registration: Some(provider),
                coordination,
                image_registration_generation: 1,
            },
        );
        Ok(None)
    }

    /// Atomically removes one provider registration.
    pub fn remove_provider(
        &self,
        provider: &crate::ProviderId,
    ) -> Option<Arc<ProviderRegistration>> {
        let mut providers = write_unpoisoned(&self.inner.providers);
        if let Some(slot) = providers.get_mut(provider) {
            slot.coordination.supersede_refresh();
            let previous = slot.registration.take();
            slot.image_registration_generation =
                slot.image_registration_generation.saturating_add(1);
            return previous;
        }
        let coordination = Arc::new(ProviderRefreshCoordination::new());
        coordination.supersede_refresh();
        providers.insert(
            provider.clone(),
            ProviderSlot {
                registration: None,
                coordination,
                image_registration_generation: 1,
            },
        );
        None
    }

    /// Atomically clears all provider registrations.
    pub fn clear_providers(&self) {
        let mut providers = write_unpoisoned(&self.inner.providers);
        for slot in providers.values() {
            if slot.registration.is_some() {
                slot.coordination.supersede_refresh();
            }
        }
        for slot in providers.values_mut() {
            if slot.registration.take().is_some() {
                slot.image_registration_generation =
                    slot.image_registration_generation.saturating_add(1);
            }
        }
    }

    /// Recomposes every provider from the override store without writing a
    /// flattened catalog to [`ModelsStore`]. Failed providers retain their
    /// previously published complete snapshot.
    pub fn refresh_host_overrides(&self) -> BTreeMap<crate::ProviderId, Result<(), CatalogError>> {
        self.providers()
            .iter()
            .map(|provider| {
                let result = provider.catalog.catalog_state().map_or_else(
                    || Ok(()),
                    |state| {
                        self.inner
                            .override_store
                            .snapshot(&provider.descriptor.id)
                            .map_err(CatalogError::from)
                            .and_then(|overrides| state.replace_host_overrides(overrides))
                    },
                );
                (provider.descriptor.id.clone(), result)
            })
            .collect()
    }

    /// Replaces process-local overrides for one provider and atomically
    /// publishes the recomposed effective snapshot.
    pub fn set_runtime_overrides(
        &self,
        provider: &crate::ProviderId,
        overrides: Vec<ModelOverride>,
    ) -> Result<(), CatalogError> {
        let registration = self
            .provider(provider)
            .ok_or_else(|| CatalogError::validation(format!("unknown provider: {provider}")))?;
        registration
            .catalog
            .catalog_state()
            .ok_or_else(|| {
                CatalogError::validation(format!(
                    "provider {provider} uses an unmanaged custom catalog"
                ))
            })?
            .replace_runtime_overrides(Arc::from(overrides))
    }

    /// Clears process-local overrides for one provider.
    pub fn clear_runtime_overrides(
        &self,
        provider: &crate::ProviderId,
    ) -> Result<(), CatalogError> {
        self.set_runtime_overrides(provider, Vec::new())
    }

    /// Restores and refreshes selected dynamic providers concurrently. Static
    /// and aborted providers are omitted; other failures are reported per
    /// provider.
    pub async fn refresh(
        &self,
        request: RefreshRequest,
        cancellation: CancellationToken,
    ) -> RefreshReport {
        // Pinned pi returns before provider selection when the caller arrives
        // already cancelled. In particular, it must not begin a generation
        // that supersedes an unrelated in-flight refresh.
        if cancellation.is_cancelled() {
            return RefreshReport {
                aborted: true,
                providers: BTreeMap::new(),
            };
        }

        let mut pending = FuturesUnordered::new();
        for provider in self.providers().iter().cloned() {
            if request
                .providers
                .as_ref()
                .is_some_and(|selected| !selected.contains(&provider.descriptor.id))
            {
                continue;
            }
            // Pinned pi selects only providers that expose refreshModels;
            // static providers are absent from the per-provider report.
            if provider.catalog.catalog_source().is_none() {
                continue;
            }
            pending.push(self.refresh_provider(provider, request.clone(), cancellation.clone()));
        }

        let mut providers = BTreeMap::new();
        while let Some((provider, result)) = pending.next().await {
            if let Some(result) = result {
                providers.insert(provider, result);
            }
        }
        RefreshReport {
            aborted: cancellation.is_cancelled(),
            providers,
        }
    }

    async fn refresh_provider(
        &self,
        provider: Arc<ProviderRegistration>,
        request: RefreshRequest,
        cancellation: CancellationToken,
    ) -> (crate::ProviderId, Option<ProviderRefreshResult>) {
        let provider_id = provider.descriptor.id.clone();
        let Some(source) = provider.catalog.catalog_source() else {
            return (provider_id, None);
        };
        let Some(state) = provider.catalog.catalog_state() else {
            return (provider_id, None);
        };
        let Some((generation, operation_cancellation)) =
            self.begin_provider_refresh(&provider, state.as_ref(), &cancellation)
        else {
            return (provider_id, None);
        };
        let result = self
            .refresh_provider_generation(
                &provider,
                source,
                generation,
                &request,
                operation_cancellation.clone(),
            )
            .await;
        state.finish_refresh(generation);
        // Pinned pi suppresses provider errors whenever that provider's
        // composed signal is aborted, including superseded generations.
        let result = (!operation_cancellation.is_cancelled()).then_some(result);
        (provider_id, result)
    }

    fn begin_provider_refresh(
        &self,
        provider: &Arc<ProviderRegistration>,
        state: &ProviderCatalogState,
        cancellation: &CancellationToken,
    ) -> Option<(RefreshGeneration, CancellationToken)> {
        let providers = read_unpoisoned(&self.inner.providers);
        let slot = providers.get(&provider.descriptor.id)?;
        let registration = slot.registration.as_ref()?;
        if !Arc::ptr_eq(registration, provider) {
            return None;
        }
        // Holding the registry read lock through begin_refresh makes provider
        // replacement linearizable: set_provider either cancels this exact
        // generation afterward or installs first and makes this Arc stale.
        Some(state.begin_refresh(cancellation))
    }

    async fn refresh_provider_generation(
        &self,
        provider: &ProviderRegistration,
        source: Arc<dyn crate::ModelCatalogSource>,
        generation: RefreshGeneration,
        request: &RefreshRequest,
        cancellation: CancellationToken,
    ) -> ProviderRefreshResult {
        let Some(state) = provider.catalog.catalog_state() else {
            return ProviderRefreshResult::NotRefreshable;
        };
        let state = state.as_ref();
        let retained = || state.published_snapshot().models.len();
        let failed = |error: CatalogError| ProviderRefreshResult::Failed {
            restored_model_count: retained(),
            error: error.report(),
        };

        let persisted = match await_catalog_or_cancelled(
            self.inner
                .models_store
                .read(&provider.descriptor.id, cancellation.clone()),
            &cancellation,
        )
        .await
        {
            Ok(Ok(value)) => value.map(Arc::new),
            Ok(Err(error)) => return failed(CatalogError::from(error)),
            Err(error) => return failed(error),
        };

        if let Some(snapshot) = &persisted
            && let Err(error) = restore_persisted_candidate(
                state,
                generation,
                snapshot,
                self.inner.override_store.as_ref(),
                cancellation.clone(),
            )
            .await
        {
            return failed(error);
        }

        if let Err(error) = state.verify_generation(generation, &cancellation) {
            return failed(error);
        }

        if !request.allow_network {
            return ProviderRefreshResult::RestoredOnly {
                model_count: retained(),
            };
        }

        // Pi restores cached provider state before any credential resolution.
        let auth = match await_catalog_or_cancelled(
            provider.auth.resolve(
                crate::ResolveAuthRequest {
                    provider: provider.descriptor.clone(),
                    model: None,
                    purpose: AuthResolutionPurpose::CatalogRefresh,
                    credential_store: Arc::clone(&self.inner.credentials),
                    auth_context: Arc::clone(&self.inner.auth_context),
                    overrides: AuthResolutionOverrides::default(),
                },
                cancellation.clone(),
            ),
            &cancellation,
        )
        .await
        {
            Ok(Ok(Some(auth))) => auth,
            Ok(Ok(None)) => {
                return ProviderRefreshResult::RestoredOnly {
                    model_count: retained(),
                };
            }
            Ok(Err(error)) => return failed(CatalogError::authentication(error.to_string())),
            Err(error) => return failed(error),
        };

        let old_revision = state.published_snapshot().revision.clone();
        let candidate = match await_catalog_or_cancelled(
            source.fetch(
                CatalogFetchContext {
                    provider: provider.descriptor.id.clone(),
                    stored: persisted,
                    auth,
                    force: request.force,
                },
                cancellation.clone(),
            ),
            &cancellation,
        )
        .await
        {
            Ok(Ok(candidate)) => candidate,
            Ok(Err(error)) | Err(error) => return failed(error),
        };
        let new_revision = candidate.revision.clone();
        match publish_candidate(
            state,
            generation,
            candidate,
            self.inner.models_store.as_ref(),
            self.inner.override_store.as_ref(),
            cancellation,
        )
        .await
        {
            Ok(true) => ProviderRefreshResult::Refreshed {
                old_revision,
                new_revision,
                model_count: retained(),
            },
            Ok(false) => failed(CatalogError::superseded()),
            Err(error) => failed(error),
        }
    }

    /// Executes the complete simple request pipeline and releases the registry
    /// lock before authentication or any other await point.
    pub fn stream_simple(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<crate::AssistantStream, RequestStartError>> {
        self.stream_simple_with_auth(request, AuthResolutionOverrides::default(), cancellation)
    }

    /// Executes the complete request pipeline with explicit auth overrides.
    /// This keeps secret request auth out of the serializable
    /// [`crate::SimpleGenerationOptions`] schema.
    pub fn stream_simple_with_auth(
        &self,
        mut request: ModelRequest,
        auth_overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<crate::AssistantStream, RequestStartError>> {
        Box::pin(async move {
            if request.options.cache_retention.is_none() && !cancellation.is_cancelled() {
                let environment = await_or_cancelled(
                    self.inner
                        .auth_context
                        .env("PI_CACHE_RETENTION".into(), cancellation.clone()),
                    &cancellation,
                )
                .await?
                .map_err(|error| {
                    RequestStartError::new(
                        RequestStartErrorKind::RuntimeUnavailable,
                        error.to_string(),
                    )
                    .with_model(request.model.clone())
                })?;
                if environment.as_deref() == Some("long") {
                    request.options.cache_retention = Some(CacheRetention::Long);
                }
            }
            let request_options = ApiRequestOptions::from(&request.options);
            self.stream_request_with_auth(
                request,
                None,
                request_options,
                auth_overrides,
                cancellation,
            )
            .await
        })
    }

    /// Executes fully API-specific options through the registered provider
    /// pipeline without invoking [`ApiFamily::lower_simple`].
    pub fn stream_api<A: ApiFamily>(
        &self,
        model: ModelRef,
        context: Context,
        options: A::FullOptions,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<crate::AssistantStream, RequestStartError>> {
        self.stream_api_with_request_options::<A>(
            model,
            context,
            options,
            ApiRequestOptions::default(),
            cancellation,
        )
    }

    /// Executes fully API-specific options with common retry, timeout,
    /// session-affinity, and request-header controls.
    pub fn stream_api_with_request_options<A: ApiFamily>(
        &self,
        model: ModelRef,
        context: Context,
        options: A::FullOptions,
        request_options: ApiRequestOptions,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<crate::AssistantStream, RequestStartError>> {
        self.stream_request_with_auth(
            ModelRequest {
                model,
                context,
                options: SimpleGenerationOptions::default(),
            },
            Some(ErasedApiFullOptions::new::<A>(options)),
            request_options,
            AuthResolutionOverrides::default(),
            cancellation,
        )
    }

    /// Polls or long-polls a provider-owned deferred response and returns its
    /// terminal assistant message. A still-pending result has finish reason
    /// [`crate::AssistantFinishReason::Deferred`] and carries the same handle.
    pub fn fetch_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredFetchOptions,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantMessage, RequestStartError>> {
        self.fetch_deferred_with_auth(
            model,
            handle,
            options,
            AuthResolutionOverrides::default(),
            cancellation,
        )
    }

    /// Polls a deferred response with explicit request-scoped authentication
    /// overrides. This mirrors [`Models::stream_simple_with_auth`] and keeps
    /// secrets and provider environment values out of serializable options.
    pub fn fetch_deferred_with_auth(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredFetchOptions,
        auth_overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantMessage, RequestStartError>> {
        Box::pin(async move {
            let (implementation, request) = self
                .resolve_deferred_request(DeferredResolutionRequest {
                    model_ref: model.clone(),
                    handle,
                    wait_ms: options.wait_ms,
                    request_options: options.request,
                    auth_overrides,
                    operation: DeferredOperation::Fetch,
                    cancellation: cancellation.clone(),
                })
                .await?;
            let mut stream = implementation
                .fetch_deferred(request, cancellation)
                .await
                .map_err(AiError::into_request_start)?;
            while let Some(event) = stream.next().await {
                if let Some(message) = event.terminal_message() {
                    return Ok(message.clone());
                }
            }
            Err(RequestStartError::new(
                RequestStartErrorKind::RuntimeUnavailable,
                "deferred response stream ended without a terminal message",
            )
            .with_model(model))
        })
    }

    /// Attempts best-effort provider cancellation of a deferred response.
    pub fn cancel_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredCancelOptions,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), RequestStartError>> {
        self.cancel_deferred_with_auth(
            model,
            handle,
            options,
            AuthResolutionOverrides::default(),
            cancellation,
        )
    }

    /// Cancels a deferred response with explicit request-scoped
    /// authentication overrides.
    pub fn cancel_deferred_with_auth(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredCancelOptions,
        auth_overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), RequestStartError>> {
        Box::pin(async move {
            let (implementation, request) = self
                .resolve_deferred_request(DeferredResolutionRequest {
                    model_ref: model,
                    handle,
                    wait_ms: None,
                    request_options: options,
                    auth_overrides,
                    operation: DeferredOperation::Cancel,
                    cancellation: cancellation.clone(),
                })
                .await?;
            implementation
                .cancel_deferred(request, cancellation)
                .await
                .map_err(AiError::into_request_start)
        })
    }

    async fn resolve_deferred_request(
        &self,
        request: DeferredResolutionRequest,
    ) -> Result<(Arc<dyn crate::ChatApi>, ResolvedDeferredRequest), RequestStartError> {
        let DeferredResolutionRequest {
            model_ref,
            handle,
            wait_ms,
            request_options,
            auth_overrides,
            operation,
            cancellation,
        } = request;
        cancellation.check().map_err(|_| {
            RequestStartError::new(RequestStartErrorKind::Cancelled, "request cancelled")
                .with_model(model_ref.clone())
        })?;
        let provider = self.provider(&model_ref.provider).ok_or_else(|| {
            RequestStartError::new(
                RequestStartErrorKind::UnknownProvider,
                format!("unknown provider: {}", model_ref.provider),
            )
            .with_model(model_ref.clone())
        })?;
        let model = provider
            .catalog
            .snapshot()
            .iter()
            .find(|model| model.common.model_ref == model_ref)
            .cloned()
            .ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::UnknownModel,
                    format!("unknown model: {model_ref:?}"),
                )
                .with_model(model_ref.clone())
            })?;
        let api = model.api.api_id();
        let provider_supports_operation = provider.apis.values().any(|api| {
            let capabilities = api.deferred_capabilities();
            match operation {
                DeferredOperation::Fetch => capabilities.fetch,
                DeferredOperation::Cancel => capabilities.cancel,
            }
        });
        if !provider_supports_operation {
            return Err(RequestStartError::new(
                RequestStartErrorKind::UnsupportedOperation,
                format!(
                    "Provider {} does not support deferred responses",
                    provider.descriptor.id
                ),
            )
            .with_model(model_ref));
        }

        let resolved_auth = await_or_cancelled(
            provider.auth.resolve(
                crate::ResolveAuthRequest {
                    provider: provider.descriptor.clone(),
                    model: Some(model.clone()),
                    purpose: AuthResolutionPurpose::Request,
                    credential_store: Arc::clone(&self.inner.credentials),
                    auth_context: Arc::clone(&self.inner.auth_context),
                    overrides: auth_overrides,
                },
                cancellation.clone(),
            ),
            &cancellation,
        )
        .await?
        .map_err(|error| {
            RequestStartError::new(RequestStartErrorKind::RuntimeUnavailable, error.to_string())
                .with_model(model.common.model_ref.clone())
        })?;
        let header_only_auth = resolved_auth.is_none()
            && matches!(&model.api, crate::ApiModelConfig::OpenAiResponses(_))
            && has_responses_auth_header_spec(&request_options.headers);
        let auth = match resolved_auth {
            Some(auth) => auth,
            None if header_only_auth => crate::ResolvedAuth {
                api_key: None,
                headers: http::HeaderMap::new(),
                transport_headers: http::HeaderMap::new(),
                environment: std::collections::BTreeMap::new(),
                base_url: None,
                source: crate::AuthSource::new("explicit_header"),
            },
            None => return Err(provider_not_configured(&model_ref, &provider.descriptor.id)),
        };

        let endpoint = auth
            .base_url
            .clone()
            .or_else(|| provider.descriptor.base_url.clone())
            .unwrap_or_else(|| model.common.base_url.clone());
        let mut auth_headers = auth.transport_headers.clone();
        merge_header_map(&mut auth_headers, &auth.headers);
        let mut headers =
            provider_default_headers(&provider).map_err(AiError::into_request_start)?;
        merge_header_map(&mut headers, &auth.headers);
        apply_header_spec(&mut headers, &model.common.headers).map_err(|error| {
            RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                .with_model(model_ref.clone())
        })?;
        apply_header_spec(&mut headers, &request_options.headers).map_err(|error| {
            RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                .with_model(model_ref.clone())
        })?;
        for transform in self.inner.header_transforms.iter() {
            await_or_cancelled(
                transform.transform(
                    HeaderTransformContext {
                        provider: &provider.descriptor.id,
                        model: &model,
                        api: &api,
                        endpoint: &endpoint,
                    },
                    &mut headers,
                ),
                &cancellation,
            )
            .await?
            .map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::Internal, error.message)
                    .with_model(model_ref.clone())
            })?;
        }
        if header_only_auth && !has_responses_auth_header(&headers) {
            return Err(provider_not_configured(&model_ref, &provider.descriptor.id));
        }

        let implementation = provider.apis.get(&api).cloned();
        let target_supports_operation = implementation.as_ref().is_some_and(|implementation| {
            let capabilities = implementation.deferred_capabilities();
            match operation {
                DeferredOperation::Fetch => capabilities.fetch,
                DeferredOperation::Cancel => capabilities.cancel,
            }
        });
        if !target_supports_operation {
            let message = match operation {
                DeferredOperation::Fetch => format!(
                    "Provider {} does not support deferred responses for \"{api}\"",
                    provider.descriptor.id
                ),
                DeferredOperation::Cancel => format!(
                    "Provider {} cannot cancel deferred responses for \"{api}\"",
                    provider.descriptor.id
                ),
            };
            return Err(RequestStartError::new(
                RequestStartErrorKind::UnsupportedOperation,
                message,
            )
            .with_model(model_ref));
        }
        let implementation = implementation.expect("checked deferred API implementation");

        let mut retry_policy = provider.retry_policy.clone();
        if let Some(max_retries) = request_options.max_retries {
            retry_policy.max_retries = max_retries;
        }
        if let Some(max_delay_ms) = request_options.max_retry_delay_ms {
            retry_policy.max_server_delay = Some(Duration::from_millis(max_delay_ms));
        }
        let timeout = request_options.timeout_ms.map(Duration::from_millis);
        Ok((
            implementation,
            ResolvedDeferredRequest {
                model,
                handle,
                wait_ms,
                request_options,
                endpoint,
                headers,
                auth_headers,
                api,
                api_key: auth.api_key,
                response_observers: Arc::clone(&self.inner.response_observers),
                attempt_middleware: Arc::clone(&self.inner.attempt_middleware),
                retry_policy,
                timeout,
                retry_classifier: Arc::clone(&provider.retry_classifier),
            },
        ))
    }

    fn stream_request_with_auth(
        &self,
        request: ModelRequest,
        full_options: Option<ErasedApiFullOptions>,
        request_options: ApiRequestOptions,
        auth_overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<crate::AssistantStream, RequestStartError>> {
        Box::pin(async move {
            // The Arc clone is the only registry access in this async request.
            let provider = self.provider(&request.model.provider).ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::UnknownProvider,
                    format!("unknown provider: {}", request.model.provider),
                )
                .with_model(request.model.clone())
            })?;
            let model = provider
                .catalog
                .snapshot()
                .iter()
                .find(|model| model.common.model_ref == request.model)
                .cloned()
                .ok_or_else(|| {
                    RequestStartError::new(
                        RequestStartErrorKind::UnknownModel,
                        format!("unknown model: {:?}", request.model),
                    )
                    .with_model(request.model.clone())
                })?;

            let api = model.api.api_id();
            let implementation = provider.apis.get(&api).cloned().ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::Internal,
                    format!(
                        "provider {} has no API implementation for {api}",
                        provider.descriptor.id
                    ),
                )
                .with_model(request.model.clone())
            })?;
            if let Some(options) = full_options.as_ref()
                && options.api != api
            {
                return Err(RequestStartError::new(
                    RequestStartErrorKind::InvalidRequest,
                    format!(
                        "full API options for {} cannot be applied to model API {api}",
                        options.api
                    ),
                )
                .with_model(request.model.clone()));
            }

            if cancellation.is_cancelled() {
                return Ok(crate::provider::pre_cancelled_stream(&model, &api));
            }

            let mut auth_overrides = auth_overrides;
            if let Some(options) = full_options.as_ref() {
                implementation
                    .apply_full_options_auth_overrides(&model, options, &mut auth_overrides)
                    .map_err(AiError::into_request_start)?;
            }

            let resolved_auth = await_or_cancelled(
                provider.auth.resolve(
                    crate::ResolveAuthRequest {
                        provider: provider.descriptor.clone(),
                        model: Some(model.clone()),
                        purpose: AuthResolutionPurpose::Request,
                        credential_store: Arc::clone(&self.inner.credentials),
                        auth_context: Arc::clone(&self.inner.auth_context),
                        overrides: auth_overrides,
                    },
                    cancellation.clone(),
                ),
                &cancellation,
            )
            .await?
            .map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::RuntimeUnavailable, error.to_string())
                    .with_model(request.model.clone())
            })?;
            let header_only_auth = resolved_auth.is_none()
                && matches!(&model.api, crate::ApiModelConfig::OpenAiResponses(_))
                && has_responses_auth_header_spec(&request_options.headers);
            let auth = match resolved_auth {
                Some(auth) => auth,
                None if header_only_auth => crate::ResolvedAuth {
                    api_key: None,
                    headers: http::HeaderMap::new(),
                    transport_headers: http::HeaderMap::new(),
                    environment: std::collections::BTreeMap::new(),
                    base_url: None,
                    source: crate::AuthSource::new("explicit_header"),
                },
                None => {
                    return Err(provider_not_configured(
                        &request.model,
                        &provider.descriptor.id,
                    ));
                }
            };

            let endpoint = auth
                .base_url
                .clone()
                .or_else(|| provider.descriptor.base_url.clone())
                .unwrap_or_else(|| model.common.base_url.clone());

            // Keep credential-derived transport invariants distinct from the
            // mutable logical header overlay while also retaining ordinary
            // auth headers for transports that must reassert them.
            let mut auth_headers = auth.transport_headers.clone();
            merge_header_map(&mut auth_headers, &auth.headers);

            // Provider/auth -> model -> explicit request. HeaderValue's
            // sensitivity bit controls diagnostics only; it does not alter
            // logical-header precedence.
            let mut headers =
                provider_default_headers(&provider).map_err(AiError::into_request_start)?;
            merge_header_map(&mut headers, &auth.headers);
            if let Some(options) = full_options.as_ref()
                && !matches!(
                    &model.api,
                    crate::ApiModelConfig::OpenAiResponses(_)
                        | crate::ApiModelConfig::OpenAiCodexResponses(_)
                )
            {
                implementation
                    .apply_full_options_headers(
                        &model,
                        &request.context,
                        options,
                        &endpoint,
                        &request_options,
                        &mut headers,
                    )
                    .map_err(AiError::into_request_start)?;
            } else if let crate::ApiModelConfig::AnthropicMessages(config) = &model.api {
                apply_anthropic_messages_default_headers(
                    config,
                    &request.context,
                    &request.options,
                    &mut headers,
                )
                .map_err(|error| {
                    RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                        .with_model(request.model.clone())
                })?;
            }
            apply_header_spec(&mut headers, &model.common.headers).map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                    .with_model(request.model.clone())
            })?;
            implementation
                .apply_contextual_headers(&model, &request.context, &endpoint, &mut headers)
                .map_err(AiError::into_request_start)?;
            if let Some(options) = full_options.as_ref()
                && matches!(
                    &model.api,
                    crate::ApiModelConfig::OpenAiResponses(_)
                        | crate::ApiModelConfig::OpenAiCodexResponses(_)
                )
            {
                implementation
                    .apply_full_options_headers(
                        &model,
                        &request.context,
                        options,
                        &endpoint,
                        &request_options,
                        &mut headers,
                    )
                    .map_err(AiError::into_request_start)?;
            }
            if full_options.is_none()
                && let crate::ApiModelConfig::OpenAiCompletions(config) = &model.api
            {
                apply_openai_completions_session_affinity_headers(
                    &endpoint,
                    &config.compat,
                    &request.options,
                    &mut headers,
                )
                .map_err(|error| {
                    RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                        .with_model(request.model.clone())
                })?;
            }
            if full_options.is_none() {
                let affinity = match &model.api {
                    crate::ApiModelConfig::OpenAiResponses(config) => {
                        apply_openai_responses_session_affinity_headers(
                            &endpoint,
                            &config.compat,
                            &request.options,
                            &mut headers,
                        )
                    }
                    crate::ApiModelConfig::OpenAiCodexResponses(_) => {
                        apply_openai_codex_responses_session_affinity_headers(
                            &request.options,
                            &mut headers,
                        )
                    }
                    _ => Ok(()),
                };
                affinity.map_err(|error| {
                    RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                        .with_model(request.model.clone())
                })?;
            }
            apply_header_spec(&mut headers, &request_options.headers).map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                    .with_model(request.model.clone())
            })?;

            for transform in self.inner.header_transforms.iter() {
                let transformed = await_or_cancelled(
                    transform.transform(
                        HeaderTransformContext {
                            provider: &provider.descriptor.id,
                            model: &model,
                            api: &api,
                            endpoint: &endpoint,
                        },
                        &mut headers,
                    ),
                    &cancellation,
                )
                .await?;
                transformed.map_err(|error| {
                    RequestStartError::new(RequestStartErrorKind::Internal, error.message)
                        .with_model(request.model.clone())
                })?;
            }
            if header_only_auth && !has_responses_auth_header(&headers) {
                return Err(provider_not_configured(
                    &request.model,
                    &provider.descriptor.id,
                ));
            }

            let mut retry_policy = provider.retry_policy.clone();
            if let Some(max_retries) = request_options.max_retries {
                retry_policy.max_retries = max_retries;
            }
            if let Some(max_delay_ms) = request_options.max_retry_delay_ms {
                retry_policy.max_server_delay = Some(Duration::from_millis(max_delay_ms));
            }
            let timeout = request_options.timeout_ms.map(Duration::from_millis);

            implementation
                .stream(
                    ResolvedApiRequest {
                        model,
                        context: request.context,
                        options: request.options,
                        full_options,
                        request_options,
                        endpoint,
                        headers,
                        auth_headers,
                        api,
                        api_key: auth.api_key,
                        payload_transforms: Arc::clone(&self.inner.payload_transforms),
                        response_observers: Arc::clone(&self.inner.response_observers),
                        attempt_middleware: Arc::clone(&self.inner.attempt_middleware),
                        retry_policy,
                        timeout,
                        retry_classifier: Arc::clone(&provider.retry_classifier),
                    },
                    cancellation,
                )
                .await
                .map_err(AiError::into_request_start)
        })
    }
}

impl ModelRuntime for Models {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<crate::AssistantStream, RequestStartError>> {
        self.stream_simple(request, cancellation)
    }
}

impl DeferredModelRuntime for Models {
    fn fetch_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredFetchOptions,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantMessage, RequestStartError>> {
        Models::fetch_deferred(self, model, handle, options, cancellation)
    }

    fn cancel_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredCancelOptions,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), RequestStartError>> {
        Models::cancel_deferred(self, model, handle, options, cancellation)
    }
}

impl LocalModelRuntime for Models {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        Box::pin(async move {
            let stream = self.stream_simple(request, cancellation).await?;
            Ok(LocalAssistantStream::new(stream))
        })
    }
}

/// Immutable Models configuration builder.
pub struct ModelsBuilder {
    providers: Vec<ProviderRegistration>,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
    models_store: Arc<dyn ModelsStore>,
    override_store: Arc<dyn ModelOverrideStore>,
    header_transforms: Vec<Arc<dyn HeaderTransform>>,
    payload_transforms: Vec<Arc<dyn ErasedPayloadTransform>>,
    response_observers: Vec<Arc<dyn ResponseObserver>>,
    attempt_middleware: Vec<Arc<dyn AttemptMiddleware>>,
}

impl Default for ModelsBuilder {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            credentials: Arc::new(InMemoryCredentialStore::default()),
            auth_context: Arc::new(EmptyAuthContext),
            models_store: Arc::new(InMemoryModelsStore::default()),
            override_store: Arc::new(InMemoryModelOverrideStore::default()),
            header_transforms: Vec::new(),
            payload_transforms: Vec::new(),
            response_observers: Vec::new(),
            attempt_middleware: Vec::new(),
        }
    }
}

impl ModelsBuilder {
    /// Sets the Models-owned credential store used for request resolution,
    /// login, logout, and OAuth refresh leases.
    pub fn credential_store(mut self, store: Arc<dyn CredentialStore>) -> Self {
        self.credentials = store;
        self
    }

    /// Sets the host-owned ambient authentication context.
    pub fn auth_context(mut self, context: Arc<dyn AuthContext>) -> Self {
        self.auth_context = context;
        self
    }

    /// Sets the durable provider-originated catalog store.
    pub fn models_store(mut self, store: Arc<dyn ModelsStore>) -> Self {
        self.models_store = store;
        self
    }

    /// Sets the synchronous host-policy override store.
    pub fn model_override_store(mut self, store: Arc<dyn ModelOverrideStore>) -> Self {
        self.override_store = store;
        self
    }

    /// Adds or replaces a provider by identifier. The last registration wins
    /// while retaining the identifier's original position.
    pub fn provider(mut self, provider: ProviderRegistration) -> Self {
        self.providers.push(provider);
        self
    }

    /// Adds a Models-level header transform.
    pub fn header_transform(mut self, transform: Arc<dyn HeaderTransform>) -> Self {
        self.header_transforms.push(transform);
        self
    }

    /// Adds a logical payload transform.
    pub fn payload_transform<A: ApiFamily>(
        mut self,
        transform: Arc<dyn PayloadTransform<A>>,
    ) -> Self {
        self.payload_transforms
            .push(Arc::new(PayloadTransformAdapter::<A>::new(transform)));
        self
    }

    /// Adds an already erased logical payload transform.
    pub fn erased_payload_transform(mut self, transform: Arc<dyn ErasedPayloadTransform>) -> Self {
        self.payload_transforms.push(transform);
        self
    }

    /// Adds a response observer.
    pub fn response_observer(mut self, observer: Arc<dyn ResponseObserver>) -> Self {
        self.response_observers.push(observer);
        self
    }

    /// Adds retry-attempt middleware.
    pub fn attempt_middleware(mut self, middleware: Arc<dyn AttemptMiddleware>) -> Self {
        self.attempt_middleware.push(middleware);
        self
    }

    /// Validates every provider before publishing the complete initial map.
    pub fn build(self) -> Result<Models, ProviderRegistrationError> {
        let mut providers: IndexMap<crate::ProviderId, ProviderSlot> = IndexMap::new();
        for provider in self.providers {
            if let Some(state) = provider.catalog.catalog_state() {
                let host_overrides = self
                    .override_store
                    .snapshot(&provider.descriptor.id)
                    .map_err(|error| ProviderRegistrationError::Catalog {
                        provider: provider.descriptor.id.clone(),
                        message: error.message,
                    })?;
                state
                    .replace_host_overrides(host_overrides)
                    .map_err(|error| ProviderRegistrationError::Catalog {
                        provider: provider.descriptor.id.clone(),
                        message: error.message,
                    })?;
            }
            provider.validate()?;
            let provider = Arc::new(provider);
            let provider_id = provider.descriptor.id.clone();
            if let Some(slot) = providers.get_mut(&provider_id) {
                slot.coordination.supersede_refresh();
                if let Some(state) = provider.catalog.catalog_state() {
                    state.bind_coordination(Arc::clone(&slot.coordination));
                }
                slot.registration = Some(provider);
                slot.image_registration_generation =
                    slot.image_registration_generation.saturating_add(1);
            } else {
                let coordination = Arc::new(ProviderRefreshCoordination::new());
                if let Some(state) = provider.catalog.catalog_state() {
                    state.bind_coordination(Arc::clone(&coordination));
                }
                providers.insert(
                    provider_id,
                    ProviderSlot {
                        registration: Some(provider),
                        coordination,
                        image_registration_generation: 1,
                    },
                );
            }
        }
        Ok(Models {
            inner: Arc::new(ModelsInner {
                providers: RwLock::new(providers),
                credentials: self.credentials,
                auth_context: self.auth_context,
                models_store: self.models_store,
                override_store: self.override_store,
                header_transforms: Arc::from(self.header_transforms),
                payload_transforms: Arc::from(self.payload_transforms),
                response_observers: Arc::from(self.response_observers),
                attempt_middleware: Arc::from(self.attempt_middleware),
                image_refreshes: std::sync::Mutex::new(BTreeMap::new()),
            }),
        })
    }
}

/// Immutable snapshot of currently registered local providers.
pub type LocalProviderSnapshot = Rc<[Rc<LocalProviderRegistration>]>;

/// Immutable flattened local model snapshot.
pub type LocalModelSnapshot = Rc<[ModelDescriptor]>;

/// Immutable flattened local image-model snapshot.
pub type LocalImageModelSnapshot = Rc<[crate::ImageModelDescriptor]>;

type LocalImageRefreshFuture = Shared<
    crate::LocalBoxFuture<'static, Result<LocalImageModelSnapshot, crate::ImageCatalogError>>,
>;

struct LocalImageRefreshTask {
    future: LocalImageRefreshFuture,
}

/// Cloneable single-threaded model/provider/auth control-plane handle.
///
/// Unlike [`Models`], every provider and middleware component is held through
/// `Rc`, so browser, embedded, and main-thread hosts can retain non-`Send`
/// state end to end.
#[derive(Clone)]
pub struct LocalModels {
    inner: Rc<LocalModelsInner>,
}

struct LocalModelsInner {
    providers: RefCell<IndexMap<crate::ProviderId, LocalProviderSlot>>,
    credentials: Rc<dyn LocalCredentialStore>,
    auth_context: Rc<dyn LocalAuthContext>,
    models_store: Rc<dyn LocalModelsStore>,
    override_store: Rc<dyn LocalModelOverrideStore>,
    header_transforms: Rc<[Rc<dyn LocalHeaderTransform>]>,
    payload_transforms: Rc<[Rc<dyn LocalErasedPayloadTransform>]>,
    response_observers: Rc<[Rc<dyn LocalResponseObserver>]>,
    attempt_middleware: Rc<[Rc<dyn LocalAttemptMiddleware>]>,
    image_refreshes: RefCell<BTreeMap<ImageRefreshKey, Rc<LocalImageRefreshTask>>>,
}

struct LocalProviderSlot {
    // As in the Send registry, absence does not discard provider-ID-scoped
    // generation/publication coordination.
    registration: Option<Rc<LocalProviderRegistration>>,
    coordination: Rc<LocalProviderRefreshCoordination>,
    image_registration_generation: u64,
}

impl fmt::Debug for LocalModels {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalModels")
            .field("provider_count", &self.providers().len())
            .finish_non_exhaustive()
    }
}

impl Default for LocalModels {
    fn default() -> Self {
        Self::builder()
            .build()
            .expect("empty LocalModels registration is valid")
    }
}

impl LocalModels {
    /// Starts a local Models configuration builder.
    pub fn builder() -> LocalModelsBuilder {
        LocalModelsBuilder::default()
    }

    /// Returns the current local provider snapshot without retaining a
    /// `RefCell` borrow.
    pub fn providers(&self) -> LocalProviderSnapshot {
        Rc::from(
            self.inner
                .providers
                .borrow()
                .values()
                .filter_map(|slot| slot.registration.as_ref().map(Rc::clone))
                .collect::<Vec<_>>(),
        )
    }

    /// Returns one registered local provider handle.
    pub fn provider(&self, provider: &crate::ProviderId) -> Option<Rc<LocalProviderRegistration>> {
        self.inner
            .providers
            .borrow()
            .get(provider)
            .and_then(|slot| slot.registration.as_ref().map(Rc::clone))
    }

    /// Returns a flattened snapshot of current local catalogs.
    pub fn models(&self) -> LocalModelSnapshot {
        let providers = self.providers();
        Rc::from(
            providers
                .iter()
                .flat_map(|provider| {
                    provider
                        .catalog
                        .snapshot()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
        )
    }

    /// Returns the distinct local image-generation catalog in provider order.
    pub fn image_models(&self) -> LocalImageModelSnapshot {
        Rc::from(
            self.providers()
                .iter()
                .flat_map(|provider| provider.image_models.iter().cloned())
                .collect::<Vec<_>>(),
        )
    }

    /// Returns one local provider's current image-model snapshot
    /// synchronously without initiating a refresh.
    pub fn image_models_for(&self, provider_id: &crate::ProviderId) -> LocalImageModelSnapshot {
        self.provider(provider_id).map_or_else(
            || Rc::from(Vec::new()),
            |provider| Rc::clone(&provider.image_models),
        )
    }

    /// Looks up one local image-generation model synchronously.
    pub fn image_model(&self, model_ref: &ModelRef) -> Option<crate::ImageModelDescriptor> {
        self.provider(&model_ref.provider)?
            .image_models
            .iter()
            .find(|model| model.model_ref == *model_ref)
            .cloned()
    }

    /// Local counterpart of [`Models::refresh_image_models`].
    pub fn refresh_image_models(
        &self,
        provider_id: Option<crate::ProviderId>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), crate::ImageCatalogError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return if provider_id.is_some() {
                    Err(crate::ImageCatalogError::new(
                        "image catalog refresh cancelled",
                    ))
                } else {
                    Ok(())
                };
            }
            if let Some(provider_id) = provider_id {
                let Some((provider, generation)) =
                    self.image_provider_with_generation(&provider_id)
                else {
                    return Ok(());
                };
                self.refresh_image_provider(provider, generation, cancellation)
                    .await?;
                return Ok(());
            }
            let _ = join_all(self.image_providers_with_generations().into_iter().map(
                |(provider, generation)| {
                    let cancellation = cancellation.clone();
                    async move {
                        self.refresh_image_provider(provider, generation, cancellation)
                            .await
                    }
                },
            ))
            .await;
            Ok(())
        })
    }

    async fn refresh_image_provider(
        &self,
        provider: Rc<LocalProviderRegistration>,
        registration_generation: u64,
        cancellation: CancellationToken,
    ) -> Result<LocalImageModelSnapshot, crate::ImageCatalogError> {
        let Some(source) = provider.image_model_source.as_ref().map(Rc::clone) else {
            return Ok(Rc::clone(&provider.image_models));
        };
        let provider_id = provider.descriptor.id.clone();
        let key = (provider_id.clone(), registration_generation);
        let refresh = {
            let mut refreshes = self.inner.image_refreshes.borrow_mut();
            Rc::clone(refreshes.entry(key.clone()).or_insert_with(|| {
                let source = Rc::clone(&source);
                Rc::new(LocalImageRefreshTask {
                    future:
                        async move { source.fetch(CancellationToken::new()).await.map(Rc::from) }
                            .boxed_local()
                            .shared(),
                })
            }))
        };
        let result = match select(
            Box::pin(refresh.future.clone()),
            Box::pin(cancellation.cancelled()),
        )
        .await
        {
            Either::Left((result, _)) => result,
            Either::Right(((), _)) => {
                return Err(crate::ImageCatalogError::new(
                    "image catalog refresh cancelled",
                ));
            }
        };
        let mut refreshes = self.inner.image_refreshes.borrow_mut();
        if refreshes
            .get(&key)
            .is_some_and(|current| Rc::ptr_eq(current, &refresh))
        {
            refreshes.remove(&key);
        }
        drop(refreshes);
        let snapshot = result?;
        self.publish_image_snapshot_if_current(
            &provider,
            registration_generation,
            Rc::clone(&snapshot),
        )?;
        Ok(snapshot)
    }

    fn image_provider_with_generation(
        &self,
        provider: &crate::ProviderId,
    ) -> Option<(Rc<LocalProviderRegistration>, u64)> {
        let providers = self.inner.providers.borrow();
        let slot = providers.get(provider)?;
        Some((
            Rc::clone(slot.registration.as_ref()?),
            slot.image_registration_generation,
        ))
    }

    fn image_providers_with_generations(&self) -> Vec<(Rc<LocalProviderRegistration>, u64)> {
        self.inner
            .providers
            .borrow()
            .values()
            .filter_map(|slot| {
                slot.registration
                    .as_ref()
                    .map(|provider| (Rc::clone(provider), slot.image_registration_generation))
            })
            .collect()
    }

    fn publish_image_snapshot_if_current(
        &self,
        provider: &Rc<LocalProviderRegistration>,
        registration_generation: u64,
        snapshot: LocalImageModelSnapshot,
    ) -> Result<(), crate::ImageCatalogError> {
        let mut replacement = (**provider).clone();
        replacement.image_models = snapshot;
        replacement.validate().map_err(|error| {
            crate::ImageCatalogError::new(format!("invalid image catalog: {error}"))
        })?;
        let replacement = Rc::new(replacement);
        let mut providers = self.inner.providers.borrow_mut();
        let Some(slot) = providers.get_mut(&provider.descriptor.id) else {
            return Ok(());
        };
        if slot.image_registration_generation != registration_generation
            || !slot
                .registration
                .as_ref()
                .is_some_and(|current| Rc::ptr_eq(current, provider))
        {
            return Ok(());
        }
        slot.registration = Some(replacement);
        slot.image_registration_generation = slot.image_registration_generation.saturating_add(1);
        Ok(())
    }

    /// Generates images through a local provider capability.
    pub fn generate_images(
        &self,
        model: crate::ImageModelDescriptor,
        context: crate::ImageGenerationContext,
        options: crate::ImageGenerationOptions,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, crate::AssistantImages> {
        Box::pin(async move {
            let failure_model = model.model_ref.clone();
            let failure_api = model.api.clone();
            let failure = |reason, message: String| {
                crate::AssistantImages::failure(
                    &failure_model,
                    failure_api.clone(),
                    reason,
                    message,
                )
            };
            if cancellation.is_cancelled() {
                return failure(
                    crate::ImageGenerationStopReason::Error,
                    "Request aborted".into(),
                );
            }
            let Some(provider) = self.provider(&model.model_ref.provider) else {
                return failure(
                    crate::ImageGenerationStopReason::Error,
                    format!("Unknown provider: {}", model.model_ref.provider),
                );
            };
            let Some(api) = provider.image_apis.get(&model.api).cloned() else {
                return failure(
                    crate::ImageGenerationStopReason::Error,
                    format!(
                        "provider {} has no image API for {}",
                        provider.descriptor.id, model.api
                    ),
                );
            };
            let crate::ImageGenerationOptions {
                request: request_options,
                auth: auth_overrides,
                metadata,
            } = options;
            let request_environment = auth_overrides.environment.clone();
            let auth = match await_or_cancelled(
                provider.auth.resolve(
                    crate::LocalResolveAuthRequest {
                        provider: provider.descriptor.clone(),
                        model: None,
                        purpose: AuthResolutionPurpose::Request,
                        credential_store: Rc::clone(&self.inner.credentials),
                        auth_context: Rc::clone(&self.inner.auth_context),
                        overrides: auth_overrides,
                    },
                    cancellation.clone(),
                ),
                &cancellation,
            )
            .await
            {
                Ok(auth) => auth,
                Err(_) => {
                    return failure(
                        crate::ImageGenerationStopReason::Error,
                        "Request aborted".into(),
                    );
                }
            };
            let auth = match auth {
                Ok(Some(auth)) => auth,
                Ok(None) => crate::ResolvedAuth {
                    api_key: None,
                    headers: http::HeaderMap::new(),
                    transport_headers: http::HeaderMap::new(),
                    environment: std::collections::BTreeMap::new(),
                    base_url: None,
                    source: crate::AuthSource::new("unconfigured"),
                },
                Err(error) => {
                    return failure(crate::ImageGenerationStopReason::Error, error.to_string());
                }
            };
            if cancellation.is_cancelled() {
                return failure(
                    crate::ImageGenerationStopReason::Error,
                    "Request aborted".into(),
                );
            }
            let endpoint = auth
                .base_url
                .clone()
                .or_else(|| provider.descriptor.base_url.clone())
                .unwrap_or_else(|| model.base_url.clone());
            let mut auth_headers = auth.transport_headers.clone();
            merge_header_map(&mut auth_headers, &auth.headers);
            let mut headers = match local_provider_default_headers(provider.as_ref()) {
                Ok(headers) => headers,
                Err(error) => {
                    return failure(crate::ImageGenerationStopReason::Error, error.to_string());
                }
            };
            if let Err(error) = apply_header_spec(&mut headers, &model.headers) {
                return failure(crate::ImageGenerationStopReason::Error, error.to_string());
            }
            merge_header_map(&mut headers, &auth.headers);
            if let Err(error) = apply_header_spec(&mut headers, &request_options.headers) {
                return failure(crate::ImageGenerationStopReason::Error, error.to_string());
            }
            for transform in self.inner.header_transforms.iter() {
                match await_or_cancelled(
                    transform.transform_image(
                        ImageHeaderTransformContext {
                            provider: &provider.descriptor.id,
                            model: &model,
                            api: &model.api,
                            endpoint: &endpoint,
                        },
                        &mut headers,
                    ),
                    &cancellation,
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        return failure(crate::ImageGenerationStopReason::Error, error.to_string());
                    }
                    Err(_) => {
                        return failure(
                            crate::ImageGenerationStopReason::Error,
                            "Request aborted".into(),
                        );
                    }
                }
            }
            let mut environment = auth.environment;
            environment.extend(request_environment);
            let mut retry_policy = provider.retry_policy.clone();
            if let Some(max_retries) = request_options.max_retries {
                retry_policy.max_retries = max_retries;
            }
            if let Some(max_delay_ms) = request_options.max_retry_delay_ms {
                retry_policy.max_server_delay = Some(Duration::from_millis(max_delay_ms));
            }
            let timeout = request_options.timeout_ms.map(Duration::from_millis);
            api.generate(
                crate::LocalResolvedImageRequest {
                    model,
                    context,
                    request_options,
                    endpoint,
                    headers,
                    auth_headers,
                    environment,
                    metadata,
                    api_key: auth.api_key,
                    payload_transforms: Rc::clone(&self.inner.payload_transforms),
                    retry_policy,
                    timeout,
                    retry_classifier: Rc::clone(&provider.retry_classifier),
                    response_observers: Rc::clone(&self.inner.response_observers),
                    attempt_middleware: Rc::clone(&self.inner.attempt_middleware),
                },
                cancellation,
            )
            .await
        })
    }

    /// Applies one local provider's credential-scoped availability policy.
    pub fn filter_models(
        &self,
        provider_id: &crate::ProviderId,
        credential: Option<&Credential>,
    ) -> LocalModelSnapshot {
        let Some(provider) = self.provider(provider_id) else {
            return Rc::from(Vec::new());
        };
        let models = provider.catalog.snapshot();
        Rc::from(provider.filter_models.as_ref().map_or_else(
            || models.iter().cloned().collect(),
            |filter| filter(models.as_ref(), credential),
        ))
    }

    /// Local configuration-only provider-auth check.
    pub fn check_auth(
        &self,
        provider_id: crate::ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<crate::AuthCheck>, crate::AuthError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            let Some(provider) = self.provider(&provider_id) else {
                return Ok(None);
            };
            let credential = self
                .inner
                .credentials
                .read(provider_id, cancellation.clone())
                .await
                .map_err(crate::AuthError::from)?;
            check_local_provider_auth(
                provider.as_ref(),
                credential,
                Rc::clone(&self.inner.auth_context),
                cancellation,
            )
            .await
        })
    }

    /// Returns locally available models in provider registration order.
    pub fn get_available(
        &self,
        provider_id: Option<crate::ProviderId>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalModelSnapshot, crate::AuthError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            let providers = provider_id.map_or_else(
                || self.providers().iter().cloned().collect::<Vec<_>>(),
                |id| self.provider(&id).into_iter().collect(),
            );
            let checked = join_all(providers.iter().map(|provider| {
                let cancellation = cancellation.clone();
                async move {
                    let credential = self
                        .inner
                        .credentials
                        .read(provider.descriptor.id.clone(), cancellation.clone())
                        .await
                        .map_err(crate::AuthError::from)?;
                    let auth = check_local_provider_auth(
                        provider.as_ref(),
                        credential.clone(),
                        Rc::clone(&self.inner.auth_context),
                        cancellation,
                    )
                    .await?;
                    Ok::<_, crate::AuthError>((provider, credential, auth))
                }
            }))
            .await;

            let mut available = Vec::new();
            for result in checked {
                let (provider, credential, auth) = result?;
                if auth.is_none() {
                    continue;
                }
                let models = provider.catalog.snapshot();
                available.extend(provider.filter_models.as_ref().map_or_else(
                    || models.iter().cloned().collect(),
                    |filter| filter(models.as_ref(), credential.as_ref()),
                ));
            }
            Ok(Rc::from(available))
        })
    }

    /// Resolves a model synchronously against the latest local snapshot.
    pub fn model(&self, model_ref: &ModelRef) -> Option<ModelDescriptor> {
        let provider = self.provider(&model_ref.provider)?;
        provider
            .catalog
            .snapshot()
            .iter()
            .find(|model| model.common.model_ref == *model_ref)
            .cloned()
    }

    /// Returns the local Models-owned credential-store capability.
    pub fn credential_store(&self) -> Rc<dyn LocalCredentialStore> {
        Rc::clone(&self.inner.credentials)
    }

    /// Lists local stored credential metadata without resolving secrets.
    pub fn credential_info(
        &self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Vec<CredentialInfo>, crate::AuthError>> {
        Box::pin(async move {
            self.inner
                .credentials
                .list(cancellation)
                .await
                .map_err(crate::AuthError::from)
        })
    }

    /// Resolves local provider-scoped authentication.
    pub fn resolve_auth(
        &self,
        provider_id: crate::ProviderId,
        overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<crate::ResolvedAuth>, crate::AuthError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            let Some(provider) = self.provider(&provider_id) else {
                return Ok(None);
            };
            await_auth_or_cancelled(
                provider.auth.resolve(
                    crate::LocalResolveAuthRequest {
                        provider: provider.descriptor.clone(),
                        model: None,
                        purpose: AuthResolutionPurpose::Request,
                        credential_store: Rc::clone(&self.inner.credentials),
                        auth_context: Rc::clone(&self.inner.auth_context),
                        overrides,
                    },
                    cancellation.clone(),
                ),
                &cancellation,
            )
            .await
        })
    }

    /// Runs local provider login and persists its credential under a lease.
    pub fn login(
        &self,
        provider_id: crate::ProviderId,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Credential, crate::AuthError>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            let provider = self.provider(&provider_id).ok_or_else(|| {
                crate::AuthError::new(
                    "unknown_provider",
                    format!("unknown provider: {provider_id}"),
                )
            })?;
            let credential = await_auth_or_cancelled(
                provider.auth.login(interaction, cancellation.clone()),
                &cancellation,
            )
            .await?;
            let mut lease = self
                .inner
                .credentials
                .acquire_lease(provider_id, cancellation.clone())
                .await?;
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            lease.replace(Some(credential.clone()));
            lease.commit().await?;
            Ok(credential)
        })
    }

    /// Deletes a locally stored credential using Pi's delete-only logout
    /// behavior.
    pub fn logout(
        &self,
        provider_id: crate::ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), crate::AuthError>> {
        Box::pin(async move {
            let mut lease = self
                .inner
                .credentials
                .acquire_lease(provider_id, cancellation.clone())
                .await?;
            cancellation
                .check()
                .map_err(|_| crate::AuthError::Cancelled)?;
            lease.replace(None);
            lease.commit().await.map_err(crate::AuthError::from)
        })
    }

    /// Loads one local provider's last complete effective catalog snapshot.
    pub fn catalog_snapshot(&self, provider: &crate::ProviderId) -> Option<Rc<CatalogSnapshot>> {
        let registration = self.provider(provider)?;
        Some(
            registration
                .catalog
                .catalog_state()
                .map(|state| state.published_snapshot())
                .unwrap_or_else(|| {
                    Rc::new(CatalogSnapshot::baseline(
                        registration
                            .catalog
                            .snapshot()
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>(),
                    ))
                }),
        )
    }

    /// Returns one local provider's current provenance layers.
    pub fn catalog_layers(&self, provider: &crate::ProviderId) -> Option<ProviderCatalogLayers> {
        self.provider(provider)?
            .catalog
            .catalog_state()
            .map(|state| state.layers())
    }

    /// Atomically validates and upserts a complete local provider.
    pub fn set_provider(
        &self,
        provider: LocalProviderRegistration,
    ) -> Result<Option<Rc<LocalProviderRegistration>>, ProviderRegistrationError> {
        if let Some(state) = provider.catalog.catalog_state() {
            let host_overrides = self
                .inner
                .override_store
                .snapshot(&provider.descriptor.id)
                .map_err(|error| ProviderRegistrationError::Catalog {
                    provider: provider.descriptor.id.clone(),
                    message: error.message,
                })?;
            state
                .replace_host_overrides(host_overrides)
                .map_err(|error| ProviderRegistrationError::Catalog {
                    provider: provider.descriptor.id.clone(),
                    message: error.message,
                })?;
        }
        provider.validate()?;
        let provider = Rc::new(provider);
        let provider_id = provider.descriptor.id.clone();
        let mut providers = self.inner.providers.borrow_mut();
        if let Some(slot) = providers.get_mut(&provider_id) {
            slot.coordination.supersede_refresh();
            if let Some(state) = provider.catalog.catalog_state() {
                state.bind_coordination(Rc::clone(&slot.coordination));
            }
            let previous = slot.registration.replace(provider);
            slot.image_registration_generation =
                slot.image_registration_generation.saturating_add(1);
            if previous.is_none() {
                let slot = providers
                    .shift_remove(&provider_id)
                    .expect("local provider slot was just observed");
                providers.insert(provider_id, slot);
            }
            return Ok(previous);
        }

        let coordination = Rc::new(LocalProviderRefreshCoordination::new());
        if let Some(state) = provider.catalog.catalog_state() {
            state.bind_coordination(Rc::clone(&coordination));
        }
        providers.insert(
            provider_id,
            LocalProviderSlot {
                registration: Some(provider),
                coordination,
                image_registration_generation: 1,
            },
        );
        Ok(None)
    }

    /// Atomically removes one local provider registration.
    pub fn remove_provider(
        &self,
        provider: &crate::ProviderId,
    ) -> Option<Rc<LocalProviderRegistration>> {
        let mut providers = self.inner.providers.borrow_mut();
        if let Some(slot) = providers.get_mut(provider) {
            slot.coordination.supersede_refresh();
            let previous = slot.registration.take();
            slot.image_registration_generation =
                slot.image_registration_generation.saturating_add(1);
            return previous;
        }
        let coordination = Rc::new(LocalProviderRefreshCoordination::new());
        coordination.supersede_refresh();
        providers.insert(
            provider.clone(),
            LocalProviderSlot {
                registration: None,
                coordination,
                image_registration_generation: 1,
            },
        );
        None
    }

    /// Atomically clears all local provider registrations.
    pub fn clear_providers(&self) {
        let mut providers = self.inner.providers.borrow_mut();
        for slot in providers.values() {
            if slot.registration.is_some() {
                slot.coordination.supersede_refresh();
            }
        }
        for slot in providers.values_mut() {
            if slot.registration.take().is_some() {
                slot.image_registration_generation =
                    slot.image_registration_generation.saturating_add(1);
            }
        }
    }

    /// Recomposes every local provider from the local override store without
    /// persisting a flattened effective catalog.
    pub fn refresh_host_overrides(&self) -> BTreeMap<crate::ProviderId, Result<(), CatalogError>> {
        self.providers()
            .iter()
            .map(|provider| {
                let result = provider.catalog.catalog_state().map_or_else(
                    || Ok(()),
                    |state| {
                        self.inner
                            .override_store
                            .snapshot(&provider.descriptor.id)
                            .map_err(CatalogError::from)
                            .and_then(|overrides| state.replace_host_overrides(overrides))
                    },
                );
                (provider.descriptor.id.clone(), result)
            })
            .collect()
    }

    /// Replaces process-local overrides for one local provider.
    pub fn set_runtime_overrides(
        &self,
        provider: &crate::ProviderId,
        overrides: Vec<ModelOverride>,
    ) -> Result<(), CatalogError> {
        let registration = self
            .provider(provider)
            .ok_or_else(|| CatalogError::validation(format!("unknown provider: {provider}")))?;
        registration
            .catalog
            .catalog_state()
            .ok_or_else(|| {
                CatalogError::validation(format!(
                    "provider {provider} uses an unmanaged custom local catalog"
                ))
            })?
            .replace_runtime_overrides(Rc::from(overrides))
    }

    /// Clears process-local overrides for one local provider.
    pub fn clear_runtime_overrides(
        &self,
        provider: &crate::ProviderId,
    ) -> Result<(), CatalogError> {
        self.set_runtime_overrides(provider, Vec::new())
    }

    /// Restores and refreshes selected dynamic local providers concurrently on
    /// the calling executor. Static and aborted providers are omitted.
    pub async fn refresh(
        &self,
        request: RefreshRequest,
        cancellation: CancellationToken,
    ) -> RefreshReport {
        if cancellation.is_cancelled() {
            return RefreshReport {
                aborted: true,
                providers: BTreeMap::new(),
            };
        }

        let mut pending = FuturesUnordered::new();
        for provider in self.providers().iter().cloned() {
            if request
                .providers
                .as_ref()
                .is_some_and(|selected| !selected.contains(&provider.descriptor.id))
            {
                continue;
            }
            if provider.catalog.catalog_source().is_none() {
                continue;
            }
            pending.push(self.refresh_provider(provider, request.clone(), cancellation.clone()));
        }

        let mut providers = BTreeMap::new();
        while let Some((provider, result)) = pending.next().await {
            if let Some(result) = result {
                providers.insert(provider, result);
            }
        }
        RefreshReport {
            aborted: cancellation.is_cancelled(),
            providers,
        }
    }

    async fn refresh_provider(
        &self,
        provider: Rc<LocalProviderRegistration>,
        request: RefreshRequest,
        cancellation: CancellationToken,
    ) -> (crate::ProviderId, Option<ProviderRefreshResult>) {
        let provider_id = provider.descriptor.id.clone();
        let Some(source) = provider.catalog.catalog_source() else {
            return (provider_id, None);
        };
        let Some(state) = provider.catalog.catalog_state() else {
            return (provider_id, None);
        };
        let Some((generation, operation_cancellation)) =
            self.begin_provider_refresh(&provider, state.as_ref(), &cancellation)
        else {
            return (provider_id, None);
        };
        let result = self
            .refresh_provider_generation(
                &provider,
                source,
                generation,
                &request,
                operation_cancellation.clone(),
            )
            .await;
        state.finish_refresh(generation);
        let result = (!operation_cancellation.is_cancelled()).then_some(result);
        (provider_id, result)
    }

    fn begin_provider_refresh(
        &self,
        provider: &Rc<LocalProviderRegistration>,
        state: &LocalProviderCatalogState,
        cancellation: &CancellationToken,
    ) -> Option<(RefreshGeneration, CancellationToken)> {
        let providers = self.inner.providers.borrow();
        let slot = providers.get(&provider.descriptor.id)?;
        let registration = slot.registration.as_ref()?;
        if !Rc::ptr_eq(registration, provider) {
            return None;
        }
        Some(state.begin_refresh(cancellation))
    }

    async fn refresh_provider_generation(
        &self,
        provider: &LocalProviderRegistration,
        source: Rc<dyn crate::LocalModelCatalogSource>,
        generation: RefreshGeneration,
        request: &RefreshRequest,
        cancellation: CancellationToken,
    ) -> ProviderRefreshResult {
        let Some(state) = provider.catalog.catalog_state() else {
            return ProviderRefreshResult::NotRefreshable;
        };
        let state = state.as_ref();
        let retained = || state.published_snapshot().models.len();
        let failed = |error: CatalogError| ProviderRefreshResult::Failed {
            restored_model_count: retained(),
            error: error.report(),
        };

        let persisted = match await_catalog_or_cancelled(
            self.inner
                .models_store
                .read(&provider.descriptor.id, cancellation.clone()),
            &cancellation,
        )
        .await
        {
            Ok(Ok(value)) => value.map(Rc::new),
            Ok(Err(error)) => return failed(CatalogError::from(error)),
            Err(error) => return failed(error),
        };

        if let Some(snapshot) = &persisted
            && let Err(error) = restore_local_persisted_candidate(
                state,
                generation,
                snapshot,
                self.inner.override_store.as_ref(),
                cancellation.clone(),
            )
            .await
        {
            return failed(error);
        }

        if let Err(error) = state.verify_generation(generation, &cancellation) {
            return failed(error);
        }
        if !request.allow_network {
            return ProviderRefreshResult::RestoredOnly {
                model_count: retained(),
            };
        }

        let auth = match await_catalog_or_cancelled(
            provider.auth.resolve(
                crate::LocalResolveAuthRequest {
                    provider: provider.descriptor.clone(),
                    model: None,
                    purpose: AuthResolutionPurpose::CatalogRefresh,
                    credential_store: Rc::clone(&self.inner.credentials),
                    auth_context: Rc::clone(&self.inner.auth_context),
                    overrides: AuthResolutionOverrides::default(),
                },
                cancellation.clone(),
            ),
            &cancellation,
        )
        .await
        {
            Ok(Ok(Some(auth))) => auth,
            Ok(Ok(None)) => {
                return ProviderRefreshResult::RestoredOnly {
                    model_count: retained(),
                };
            }
            Ok(Err(error)) => return failed(CatalogError::authentication(error.to_string())),
            Err(error) => return failed(error),
        };

        let old_revision = state.published_snapshot().revision.clone();
        let stored = persisted.map(|snapshot| Arc::new((*snapshot).clone()));
        let candidate = match await_catalog_or_cancelled(
            source.fetch(
                CatalogFetchContext {
                    provider: provider.descriptor.id.clone(),
                    stored,
                    auth,
                    force: request.force,
                },
                cancellation.clone(),
            ),
            &cancellation,
        )
        .await
        {
            Ok(Ok(candidate)) => candidate,
            Ok(Err(error)) | Err(error) => return failed(error),
        };
        let new_revision = candidate.revision.clone();
        match publish_local_candidate(
            state,
            generation,
            candidate,
            self.inner.models_store.as_ref(),
            self.inner.override_store.as_ref(),
            cancellation,
        )
        .await
        {
            Ok(true) => ProviderRefreshResult::Refreshed {
                old_revision,
                new_revision,
                model_count: retained(),
            },
            Ok(false) => failed(CatalogError::superseded()),
            Err(error) => failed(error),
        }
    }

    /// Executes the local simple request pipeline without retaining a registry
    /// borrow across authentication or middleware awaits.
    pub fn stream_simple(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        self.stream_simple_with_auth(request, AuthResolutionOverrides::default(), cancellation)
    }

    /// Executes the local request pipeline with explicit auth overrides.
    /// This mirrors [`Models::stream_simple_with_auth`] without placing
    /// secrets in the serializable [`crate::SimpleGenerationOptions`] schema.
    pub fn stream_simple_with_auth(
        &self,
        mut request: ModelRequest,
        auth_overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        Box::pin(async move {
            if request.options.cache_retention.is_none() && !cancellation.is_cancelled() {
                let environment = await_or_cancelled(
                    self.inner
                        .auth_context
                        .env("PI_CACHE_RETENTION".into(), cancellation.clone()),
                    &cancellation,
                )
                .await?
                .map_err(|error| {
                    RequestStartError::new(
                        RequestStartErrorKind::RuntimeUnavailable,
                        error.to_string(),
                    )
                    .with_model(request.model.clone())
                })?;
                if environment.as_deref() == Some("long") {
                    request.options.cache_retention = Some(CacheRetention::Long);
                }
            }
            let request_options = ApiRequestOptions::from(&request.options);
            self.stream_request_with_auth(
                request,
                None,
                request_options,
                auth_overrides,
                cancellation,
            )
            .await
        })
    }

    /// Executes fully API-specific options through a registered local
    /// provider without invoking simple lowering.
    pub fn stream_api<A: ApiFamily>(
        &self,
        model: ModelRef,
        context: Context,
        options: A::FullOptions,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        self.stream_api_with_request_options::<A>(
            model,
            context,
            options,
            ApiRequestOptions::default(),
            cancellation,
        )
    }

    /// Executes fully API-specific local options with common transport
    /// controls.
    pub fn stream_api_with_request_options<A: ApiFamily>(
        &self,
        model: ModelRef,
        context: Context,
        options: A::FullOptions,
        request_options: ApiRequestOptions,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        self.stream_request_with_auth(
            ModelRequest {
                model,
                context,
                options: SimpleGenerationOptions::default(),
            },
            Some(ErasedApiFullOptions::new::<A>(options)),
            request_options,
            AuthResolutionOverrides::default(),
            cancellation,
        )
    }

    /// Local counterpart to [`Models::fetch_deferred`].
    pub fn fetch_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredFetchOptions,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AssistantMessage, RequestStartError>> {
        self.fetch_deferred_with_auth(
            model,
            handle,
            options,
            AuthResolutionOverrides::default(),
            cancellation,
        )
    }

    /// Local deferred fetch with explicit request-scoped authentication
    /// overrides.
    pub fn fetch_deferred_with_auth(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredFetchOptions,
        auth_overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AssistantMessage, RequestStartError>> {
        Box::pin(async move {
            let (implementation, request) = self
                .resolve_deferred_request(DeferredResolutionRequest {
                    model_ref: model.clone(),
                    handle,
                    wait_ms: options.wait_ms,
                    request_options: options.request,
                    auth_overrides,
                    operation: DeferredOperation::Fetch,
                    cancellation: cancellation.clone(),
                })
                .await?;
            let mut stream = implementation
                .fetch_deferred(request, cancellation)
                .await
                .map_err(AiError::into_request_start)?;
            while let Some(event) = stream.next().await {
                if let Some(message) = event.terminal_message() {
                    return Ok(message.clone());
                }
            }
            Err(RequestStartError::new(
                RequestStartErrorKind::RuntimeUnavailable,
                "deferred response stream ended without a terminal message",
            )
            .with_model(model))
        })
    }

    /// Local counterpart to [`Models::cancel_deferred`].
    pub fn cancel_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredCancelOptions,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), RequestStartError>> {
        self.cancel_deferred_with_auth(
            model,
            handle,
            options,
            AuthResolutionOverrides::default(),
            cancellation,
        )
    }

    /// Local deferred cancellation with explicit request-scoped
    /// authentication overrides.
    pub fn cancel_deferred_with_auth(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredCancelOptions,
        auth_overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), RequestStartError>> {
        Box::pin(async move {
            let (implementation, request) = self
                .resolve_deferred_request(DeferredResolutionRequest {
                    model_ref: model,
                    handle,
                    wait_ms: None,
                    request_options: options,
                    auth_overrides,
                    operation: DeferredOperation::Cancel,
                    cancellation: cancellation.clone(),
                })
                .await?;
            implementation
                .cancel_deferred(request, cancellation)
                .await
                .map_err(AiError::into_request_start)
        })
    }

    async fn resolve_deferred_request(
        &self,
        request: DeferredResolutionRequest,
    ) -> Result<(Rc<dyn crate::LocalChatApi>, LocalResolvedDeferredRequest), RequestStartError>
    {
        let DeferredResolutionRequest {
            model_ref,
            handle,
            wait_ms,
            request_options,
            auth_overrides,
            operation,
            cancellation,
        } = request;
        cancellation.check().map_err(|_| {
            RequestStartError::new(RequestStartErrorKind::Cancelled, "request cancelled")
                .with_model(model_ref.clone())
        })?;
        let provider = self.provider(&model_ref.provider).ok_or_else(|| {
            RequestStartError::new(
                RequestStartErrorKind::UnknownProvider,
                format!("unknown provider: {}", model_ref.provider),
            )
            .with_model(model_ref.clone())
        })?;
        let model = provider
            .catalog
            .snapshot()
            .iter()
            .find(|model| model.common.model_ref == model_ref)
            .cloned()
            .ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::UnknownModel,
                    format!("unknown model: {model_ref:?}"),
                )
                .with_model(model_ref.clone())
            })?;
        let api = model.api.api_id();
        let provider_supports_operation = provider.apis.values().any(|api| {
            let capabilities = api.deferred_capabilities();
            match operation {
                DeferredOperation::Fetch => capabilities.fetch,
                DeferredOperation::Cancel => capabilities.cancel,
            }
        });
        if !provider_supports_operation {
            return Err(RequestStartError::new(
                RequestStartErrorKind::UnsupportedOperation,
                format!(
                    "Provider {} does not support deferred responses",
                    provider.descriptor.id
                ),
            )
            .with_model(model_ref));
        }

        let resolved_auth = await_or_cancelled(
            provider.auth.resolve(
                crate::LocalResolveAuthRequest {
                    provider: provider.descriptor.clone(),
                    model: Some(model.clone()),
                    purpose: AuthResolutionPurpose::Request,
                    credential_store: Rc::clone(&self.inner.credentials),
                    auth_context: Rc::clone(&self.inner.auth_context),
                    overrides: auth_overrides,
                },
                cancellation.clone(),
            ),
            &cancellation,
        )
        .await?
        .map_err(|error| {
            RequestStartError::new(RequestStartErrorKind::RuntimeUnavailable, error.to_string())
                .with_model(model.common.model_ref.clone())
        })?;
        let header_only_auth = resolved_auth.is_none()
            && matches!(&model.api, crate::ApiModelConfig::OpenAiResponses(_))
            && has_responses_auth_header_spec(&request_options.headers);
        let auth = match resolved_auth {
            Some(auth) => auth,
            None if header_only_auth => crate::ResolvedAuth {
                api_key: None,
                headers: http::HeaderMap::new(),
                transport_headers: http::HeaderMap::new(),
                environment: std::collections::BTreeMap::new(),
                base_url: None,
                source: crate::AuthSource::new("explicit_header"),
            },
            None => return Err(provider_not_configured(&model_ref, &provider.descriptor.id)),
        };

        let endpoint = auth
            .base_url
            .clone()
            .or_else(|| provider.descriptor.base_url.clone())
            .unwrap_or_else(|| model.common.base_url.clone());
        let mut auth_headers = auth.transport_headers.clone();
        merge_header_map(&mut auth_headers, &auth.headers);
        let mut headers =
            local_provider_default_headers(&provider).map_err(AiError::into_request_start)?;
        merge_header_map(&mut headers, &auth.headers);
        apply_header_spec(&mut headers, &model.common.headers).map_err(|error| {
            RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                .with_model(model_ref.clone())
        })?;
        apply_header_spec(&mut headers, &request_options.headers).map_err(|error| {
            RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                .with_model(model_ref.clone())
        })?;
        for transform in self.inner.header_transforms.iter() {
            await_or_cancelled(
                transform.transform(
                    HeaderTransformContext {
                        provider: &provider.descriptor.id,
                        model: &model,
                        api: &api,
                        endpoint: &endpoint,
                    },
                    &mut headers,
                ),
                &cancellation,
            )
            .await?
            .map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::Internal, error.message)
                    .with_model(model_ref.clone())
            })?;
        }
        if header_only_auth && !has_responses_auth_header(&headers) {
            return Err(provider_not_configured(&model_ref, &provider.descriptor.id));
        }

        let implementation = provider.apis.get(&api).cloned();
        let target_supports_operation = implementation.as_ref().is_some_and(|implementation| {
            let capabilities = implementation.deferred_capabilities();
            match operation {
                DeferredOperation::Fetch => capabilities.fetch,
                DeferredOperation::Cancel => capabilities.cancel,
            }
        });
        if !target_supports_operation {
            let message = match operation {
                DeferredOperation::Fetch => format!(
                    "Provider {} does not support deferred responses for \"{api}\"",
                    provider.descriptor.id
                ),
                DeferredOperation::Cancel => format!(
                    "Provider {} cannot cancel deferred responses for \"{api}\"",
                    provider.descriptor.id
                ),
            };
            return Err(RequestStartError::new(
                RequestStartErrorKind::UnsupportedOperation,
                message,
            )
            .with_model(model_ref));
        }
        let implementation = implementation.expect("checked local deferred API implementation");

        let mut retry_policy = provider.retry_policy.clone();
        if let Some(max_retries) = request_options.max_retries {
            retry_policy.max_retries = max_retries;
        }
        if let Some(max_delay_ms) = request_options.max_retry_delay_ms {
            retry_policy.max_server_delay = Some(Duration::from_millis(max_delay_ms));
        }
        let timeout = request_options.timeout_ms.map(Duration::from_millis);
        Ok((
            implementation,
            LocalResolvedDeferredRequest {
                model,
                handle,
                wait_ms,
                request_options,
                endpoint,
                headers,
                auth_headers,
                api,
                api_key: auth.api_key,
                response_observers: Rc::clone(&self.inner.response_observers),
                attempt_middleware: Rc::clone(&self.inner.attempt_middleware),
                retry_policy,
                timeout,
                retry_classifier: Rc::clone(&provider.retry_classifier),
            },
        ))
    }

    fn stream_request_with_auth(
        &self,
        request: ModelRequest,
        full_options: Option<ErasedApiFullOptions>,
        request_options: ApiRequestOptions,
        auth_overrides: AuthResolutionOverrides,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        Box::pin(async move {
            let provider = self.provider(&request.model.provider).ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::UnknownProvider,
                    format!("unknown provider: {}", request.model.provider),
                )
                .with_model(request.model.clone())
            })?;
            let model = provider
                .catalog
                .snapshot()
                .iter()
                .find(|model| model.common.model_ref == request.model)
                .cloned()
                .ok_or_else(|| {
                    RequestStartError::new(
                        RequestStartErrorKind::UnknownModel,
                        format!("unknown model: {:?}", request.model),
                    )
                    .with_model(request.model.clone())
                })?;

            let api = model.api.api_id();
            let implementation = provider.apis.get(&api).cloned().ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::Internal,
                    format!(
                        "provider {} has no local API implementation for {api}",
                        provider.descriptor.id
                    ),
                )
                .with_model(request.model.clone())
            })?;
            if let Some(options) = full_options.as_ref()
                && options.api != api
            {
                return Err(RequestStartError::new(
                    RequestStartErrorKind::InvalidRequest,
                    format!(
                        "full API options for {} cannot be applied to model API {api}",
                        options.api
                    ),
                )
                .with_model(request.model.clone()));
            }

            if cancellation.is_cancelled() {
                return Ok(crate::provider::local_pre_cancelled_stream(&model, &api));
            }

            let mut auth_overrides = auth_overrides;
            if let Some(options) = full_options.as_ref() {
                implementation
                    .apply_full_options_auth_overrides(&model, options, &mut auth_overrides)
                    .map_err(AiError::into_request_start)?;
            }

            let resolved_auth = await_or_cancelled(
                provider.auth.resolve(
                    crate::LocalResolveAuthRequest {
                        provider: provider.descriptor.clone(),
                        model: Some(model.clone()),
                        purpose: AuthResolutionPurpose::Request,
                        credential_store: Rc::clone(&self.inner.credentials),
                        auth_context: Rc::clone(&self.inner.auth_context),
                        overrides: auth_overrides,
                    },
                    cancellation.clone(),
                ),
                &cancellation,
            )
            .await?
            .map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::RuntimeUnavailable, error.to_string())
                    .with_model(request.model.clone())
            })?;
            let header_only_auth = resolved_auth.is_none()
                && matches!(&model.api, crate::ApiModelConfig::OpenAiResponses(_))
                && has_responses_auth_header_spec(&request_options.headers);
            let auth = match resolved_auth {
                Some(auth) => auth,
                None if header_only_auth => crate::ResolvedAuth {
                    api_key: None,
                    headers: http::HeaderMap::new(),
                    transport_headers: http::HeaderMap::new(),
                    environment: std::collections::BTreeMap::new(),
                    base_url: None,
                    source: crate::AuthSource::new("explicit_header"),
                },
                None => {
                    return Err(provider_not_configured(
                        &request.model,
                        &provider.descriptor.id,
                    ));
                }
            };

            let endpoint = auth
                .base_url
                .clone()
                .or_else(|| provider.descriptor.base_url.clone())
                .unwrap_or_else(|| model.common.base_url.clone());

            let mut auth_headers = auth.transport_headers.clone();
            merge_header_map(&mut auth_headers, &auth.headers);

            let mut headers =
                local_provider_default_headers(&provider).map_err(AiError::into_request_start)?;
            merge_header_map(&mut headers, &auth.headers);
            if let Some(options) = full_options.as_ref()
                && !matches!(
                    &model.api,
                    crate::ApiModelConfig::OpenAiResponses(_)
                        | crate::ApiModelConfig::OpenAiCodexResponses(_)
                )
            {
                implementation
                    .apply_full_options_headers(
                        &model,
                        &request.context,
                        options,
                        &endpoint,
                        &request_options,
                        &mut headers,
                    )
                    .map_err(AiError::into_request_start)?;
            } else if let crate::ApiModelConfig::AnthropicMessages(config) = &model.api {
                apply_anthropic_messages_default_headers(
                    config,
                    &request.context,
                    &request.options,
                    &mut headers,
                )
                .map_err(|error| {
                    RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                        .with_model(request.model.clone())
                })?;
            }
            apply_header_spec(&mut headers, &model.common.headers).map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                    .with_model(request.model.clone())
            })?;
            implementation
                .apply_contextual_headers(&model, &request.context, &endpoint, &mut headers)
                .map_err(AiError::into_request_start)?;
            if let Some(options) = full_options.as_ref()
                && matches!(
                    &model.api,
                    crate::ApiModelConfig::OpenAiResponses(_)
                        | crate::ApiModelConfig::OpenAiCodexResponses(_)
                )
            {
                implementation
                    .apply_full_options_headers(
                        &model,
                        &request.context,
                        options,
                        &endpoint,
                        &request_options,
                        &mut headers,
                    )
                    .map_err(AiError::into_request_start)?;
            }
            if full_options.is_none()
                && let crate::ApiModelConfig::OpenAiCompletions(config) = &model.api
            {
                apply_openai_completions_session_affinity_headers(
                    &endpoint,
                    &config.compat,
                    &request.options,
                    &mut headers,
                )
                .map_err(|error| {
                    RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                        .with_model(request.model.clone())
                })?;
            }
            if full_options.is_none() {
                let affinity = match &model.api {
                    crate::ApiModelConfig::OpenAiResponses(config) => {
                        apply_openai_responses_session_affinity_headers(
                            &endpoint,
                            &config.compat,
                            &request.options,
                            &mut headers,
                        )
                    }
                    crate::ApiModelConfig::OpenAiCodexResponses(_) => {
                        apply_openai_codex_responses_session_affinity_headers(
                            &request.options,
                            &mut headers,
                        )
                    }
                    _ => Ok(()),
                };
                affinity.map_err(|error| {
                    RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                        .with_model(request.model.clone())
                })?;
            }
            apply_header_spec(&mut headers, &request_options.headers).map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                    .with_model(request.model.clone())
            })?;

            for transform in self.inner.header_transforms.iter() {
                let transformed = await_or_cancelled(
                    transform.transform(
                        HeaderTransformContext {
                            provider: &provider.descriptor.id,
                            model: &model,
                            api: &api,
                            endpoint: &endpoint,
                        },
                        &mut headers,
                    ),
                    &cancellation,
                )
                .await?;
                transformed.map_err(|error| {
                    RequestStartError::new(RequestStartErrorKind::Internal, error.message)
                        .with_model(request.model.clone())
                })?;
            }
            if header_only_auth && !has_responses_auth_header(&headers) {
                return Err(provider_not_configured(
                    &request.model,
                    &provider.descriptor.id,
                ));
            }

            let mut retry_policy = provider.retry_policy.clone();
            if let Some(max_retries) = request_options.max_retries {
                retry_policy.max_retries = max_retries;
            }
            if let Some(max_delay_ms) = request_options.max_retry_delay_ms {
                retry_policy.max_server_delay = Some(Duration::from_millis(max_delay_ms));
            }

            implementation
                .stream(
                    LocalResolvedApiRequest {
                        model,
                        context: request.context,
                        timeout: request_options.timeout_ms.map(Duration::from_millis),
                        options: request.options,
                        full_options,
                        request_options,
                        endpoint,
                        headers,
                        auth_headers,
                        api,
                        api_key: auth.api_key,
                        payload_transforms: Rc::clone(&self.inner.payload_transforms),
                        response_observers: Rc::clone(&self.inner.response_observers),
                        attempt_middleware: Rc::clone(&self.inner.attempt_middleware),
                        retry_policy,
                        retry_classifier: Rc::clone(&provider.retry_classifier),
                    },
                    cancellation,
                )
                .await
                .map_err(AiError::into_request_start)
        })
    }
}

impl LocalModelRuntime for LocalModels {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        self.stream_simple(request, cancellation)
    }
}

impl LocalDeferredModelRuntime for LocalModels {
    fn fetch_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredFetchOptions,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AssistantMessage, RequestStartError>> {
        LocalModels::fetch_deferred(self, model, handle, options, cancellation)
    }

    fn cancel_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredCancelOptions,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), RequestStartError>> {
        LocalModels::cancel_deferred(self, model, handle, options, cancellation)
    }
}

/// Immutable local Models configuration builder.
pub struct LocalModelsBuilder {
    providers: Vec<LocalProviderRegistration>,
    credentials: Rc<dyn LocalCredentialStore>,
    auth_context: Rc<dyn LocalAuthContext>,
    models_store: Rc<dyn LocalModelsStore>,
    override_store: Rc<dyn LocalModelOverrideStore>,
    header_transforms: Vec<Rc<dyn LocalHeaderTransform>>,
    payload_transforms: Vec<Rc<dyn LocalErasedPayloadTransform>>,
    response_observers: Vec<Rc<dyn LocalResponseObserver>>,
    attempt_middleware: Vec<Rc<dyn LocalAttemptMiddleware>>,
}

impl Default for LocalModelsBuilder {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            credentials: Rc::new(LocalInMemoryCredentialStore::default()),
            auth_context: Rc::new(EmptyAuthContext),
            models_store: Rc::new(LocalInMemoryModelsStore::default()),
            override_store: Rc::new(LocalInMemoryModelOverrideStore::default()),
            header_transforms: Vec::new(),
            payload_transforms: Vec::new(),
            response_observers: Vec::new(),
            attempt_middleware: Vec::new(),
        }
    }
}

impl LocalModelsBuilder {
    /// Sets the credential store shared with local provider auth resolvers.
    pub fn credential_store(mut self, store: Rc<dyn LocalCredentialStore>) -> Self {
        self.credentials = store;
        self
    }

    /// Sets the ambient authentication context used by local providers.
    pub fn auth_context(mut self, context: Rc<dyn LocalAuthContext>) -> Self {
        self.auth_context = context;
        self
    }

    /// Sets the local durable provider-originated catalog store.
    pub fn models_store(mut self, store: Rc<dyn LocalModelsStore>) -> Self {
        self.models_store = store;
        self
    }

    /// Sets the local synchronous host-policy override store.
    pub fn model_override_store(mut self, store: Rc<dyn LocalModelOverrideStore>) -> Self {
        self.override_store = store;
        self
    }

    /// Adds or replaces a local provider by identifier.
    pub fn provider(mut self, provider: LocalProviderRegistration) -> Self {
        self.providers.push(provider);
        self
    }

    /// Adds a local Models-level header transform.
    pub fn header_transform(mut self, transform: Rc<dyn LocalHeaderTransform>) -> Self {
        self.header_transforms.push(transform);
        self
    }

    /// Adds a typed local logical payload transform.
    pub fn payload_transform<A: ApiFamily>(
        mut self,
        transform: Rc<dyn LocalPayloadTransform<A>>,
    ) -> Self {
        self.payload_transforms
            .push(Rc::new(LocalPayloadTransformAdapter::<A>::new(transform)));
        self
    }

    /// Adds an already erased local logical payload transform.
    pub fn erased_payload_transform(
        mut self,
        transform: Rc<dyn LocalErasedPayloadTransform>,
    ) -> Self {
        self.payload_transforms.push(transform);
        self
    }

    /// Adds a local response observer.
    pub fn response_observer(mut self, observer: Rc<dyn LocalResponseObserver>) -> Self {
        self.response_observers.push(observer);
        self
    }

    /// Adds local retry-attempt middleware.
    pub fn attempt_middleware(mut self, middleware: Rc<dyn LocalAttemptMiddleware>) -> Self {
        self.attempt_middleware.push(middleware);
        self
    }

    /// Validates every local provider before publishing the initial map.
    pub fn build(self) -> Result<LocalModels, ProviderRegistrationError> {
        let mut providers: IndexMap<crate::ProviderId, LocalProviderSlot> = IndexMap::new();
        for provider in self.providers {
            if let Some(state) = provider.catalog.catalog_state() {
                let host_overrides = self
                    .override_store
                    .snapshot(&provider.descriptor.id)
                    .map_err(|error| ProviderRegistrationError::Catalog {
                        provider: provider.descriptor.id.clone(),
                        message: error.message,
                    })?;
                state
                    .replace_host_overrides(host_overrides)
                    .map_err(|error| ProviderRegistrationError::Catalog {
                        provider: provider.descriptor.id.clone(),
                        message: error.message,
                    })?;
            }
            provider.validate()?;
            let provider = Rc::new(provider);
            let provider_id = provider.descriptor.id.clone();
            if let Some(slot) = providers.get_mut(&provider_id) {
                slot.coordination.supersede_refresh();
                if let Some(state) = provider.catalog.catalog_state() {
                    state.bind_coordination(Rc::clone(&slot.coordination));
                }
                slot.registration = Some(provider);
                slot.image_registration_generation =
                    slot.image_registration_generation.saturating_add(1);
            } else {
                let coordination = Rc::new(LocalProviderRefreshCoordination::new());
                if let Some(state) = provider.catalog.catalog_state() {
                    state.bind_coordination(Rc::clone(&coordination));
                }
                providers.insert(
                    provider_id,
                    LocalProviderSlot {
                        registration: Some(provider),
                        coordination,
                        image_registration_generation: 1,
                    },
                );
            }
        }
        Ok(LocalModels {
            inner: Rc::new(LocalModelsInner {
                providers: RefCell::new(providers),
                credentials: self.credentials,
                auth_context: self.auth_context,
                models_store: self.models_store,
                override_store: self.override_store,
                header_transforms: Rc::from(self.header_transforms),
                payload_transforms: Rc::from(self.payload_transforms),
                response_observers: Rc::from(self.response_observers),
                attempt_middleware: Rc::from(self.attempt_middleware),
                image_refreshes: RefCell::new(BTreeMap::new()),
            }),
        })
    }
}

async fn await_or_cancelled<T, E>(
    future: impl Future<Output = Result<T, E>>,
    cancellation: &CancellationToken,
) -> Result<Result<T, E>, RequestStartError> {
    let future = Box::pin(future);
    let cancelled = Box::pin(cancellation.cancelled());
    match select(future, cancelled).await {
        Either::Left((result, _)) => Ok(result),
        Either::Right(((), _)) => Err(RequestStartError::new(
            RequestStartErrorKind::Cancelled,
            "request cancelled",
        )),
    }
}

fn provider_not_configured(model: &ModelRef, provider: &crate::ProviderId) -> RequestStartError {
    RequestStartError::new(
        RequestStartErrorKind::RuntimeUnavailable,
        format!("provider is not configured: {provider}"),
    )
    .with_model(model.clone())
}

fn has_responses_auth_header(headers: &http::HeaderMap) -> bool {
    ["authorization", "cf-aig-authorization"]
        .into_iter()
        .filter_map(|name| headers.get(name))
        .any(|value| value.to_str().is_ok_and(|value| !value.trim().is_empty()))
}

fn has_responses_auth_header_spec(headers: &crate::HeaderMapSpec) -> bool {
    headers.iter().any(|(name, value)| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "cf-aig-authorization"
        ) && value
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    })
}

async fn check_provider_auth(
    provider: &ProviderRegistration,
    credential: Option<Credential>,
    auth_context: Arc<dyn AuthContext>,
    cancellation: CancellationToken,
) -> Result<Option<crate::AuthCheck>, crate::AuthError> {
    let credential_type = credential
        .as_ref()
        .map_or(crate::CredentialType::ApiKey, Credential::credential_type);
    let store = Arc::new(InMemoryCredentialStore::default());
    if let Some(credential) = credential {
        let mut lease = store
            .acquire_lease(provider.descriptor.id.clone(), cancellation.clone())
            .await?;
        lease.replace(Some(credential));
        lease.commit().await?;
    }
    let auth = await_auth_or_cancelled(
        provider.auth.resolve(
            crate::ResolveAuthRequest {
                provider: provider.descriptor.clone(),
                model: None,
                purpose: AuthResolutionPurpose::ConfigurationCheck,
                credential_store: store,
                auth_context,
                overrides: AuthResolutionOverrides::default(),
            },
            cancellation.clone(),
        ),
        &cancellation,
    )
    .await?;
    Ok(auth.map(|auth| crate::AuthCheck {
        source: Some(auth.source),
        credential_type,
    }))
}

async fn check_local_provider_auth(
    provider: &LocalProviderRegistration,
    credential: Option<Credential>,
    auth_context: Rc<dyn LocalAuthContext>,
    cancellation: CancellationToken,
) -> Result<Option<crate::AuthCheck>, crate::AuthError> {
    let credential_type = credential
        .as_ref()
        .map_or(crate::CredentialType::ApiKey, Credential::credential_type);
    let store = Rc::new(LocalInMemoryCredentialStore::default());
    if let Some(credential) = credential {
        let mut lease = store
            .acquire_lease(provider.descriptor.id.clone(), cancellation.clone())
            .await?;
        lease.replace(Some(credential));
        lease.commit().await?;
    }
    let auth = await_auth_or_cancelled(
        provider.auth.resolve(
            crate::LocalResolveAuthRequest {
                provider: provider.descriptor.clone(),
                model: None,
                purpose: AuthResolutionPurpose::ConfigurationCheck,
                credential_store: store,
                auth_context,
                overrides: AuthResolutionOverrides::default(),
            },
            cancellation.clone(),
        ),
        &cancellation,
    )
    .await?;
    Ok(auth.map(|auth| crate::AuthCheck {
        source: Some(auth.source),
        credential_type,
    }))
}

async fn await_auth_or_cancelled<T>(
    future: impl Future<Output = Result<T, crate::AuthError>>,
    cancellation: &CancellationToken,
) -> Result<T, crate::AuthError> {
    let future = Box::pin(future);
    let cancelled = Box::pin(cancellation.cancelled());
    match select(future, cancelled).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(crate::AuthError::Cancelled),
    }
}

async fn await_catalog_or_cancelled<T, E>(
    future: impl Future<Output = Result<T, E>>,
    cancellation: &CancellationToken,
) -> Result<Result<T, E>, CatalogError> {
    let future = Box::pin(future);
    let cancelled = Box::pin(cancellation.cancelled());
    match select(future, cancelled).await {
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
