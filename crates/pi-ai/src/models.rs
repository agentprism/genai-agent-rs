//! Cloneable Models registry and request router from Architecture v2 part 1
//! §3.6 and part 2 §2.6.

use crate::{
    AiError, ApiFamily, AttemptMiddleware, CancellationToken, ErasedPayloadTransform,
    HeaderTransform, HeaderTransformContext, LocalAssistantStream, LocalAttemptMiddleware,
    LocalBoxFuture, LocalErasedPayloadTransform, LocalHeaderTransform, LocalModelRuntime,
    LocalPayloadTransform, LocalPayloadTransformAdapter, LocalProviderRegistration,
    LocalResolvedApiRequest, LocalResponseObserver, ModelDescriptor, ModelRef, ModelRequest,
    ModelRuntime, PayloadTransform, PayloadTransformAdapter, ProviderRegistration,
    ProviderRegistrationError, RequestStartError, RequestStartErrorKind, ResolvedApiRequest,
    ResponseObserver, SendBoxFuture, apply_header_spec, local_provider_default_headers,
    merge_header_map, provider_default_headers,
};
use futures_util::future::{Either, select};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::rc::Rc;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

/// Immutable snapshot of currently registered providers in registration order.
pub type ProviderSnapshot = Arc<[Arc<ProviderRegistration>]>;

/// Immutable flattened model snapshot in provider registration order.
pub type ModelSnapshot = Arc<[ModelDescriptor]>;

/// Concrete cloneable model/provider/auth control-plane handle.
#[derive(Clone)]
pub struct Models {
    inner: Arc<ModelsInner>,
}

struct ModelsInner {
    providers: RwLock<IndexMap<crate::ProviderId, Arc<ProviderRegistration>>>,
    header_transforms: Arc<[Arc<dyn HeaderTransform>]>,
    payload_transforms: Arc<[Arc<dyn ErasedPayloadTransform>]>,
    response_observers: Arc<[Arc<dyn ResponseObserver>]>,
    attempt_middleware: Arc<[Arc<dyn AttemptMiddleware>]>,
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
            .cloned()
            .collect::<Vec<_>>();
        Arc::from(providers)
    }

    /// Returns one registered provider handle without retaining a registry
    /// read lock.
    pub fn provider(&self, provider: &crate::ProviderId) -> Option<Arc<ProviderRegistration>> {
        read_unpoisoned(&self.inner.providers)
            .get(provider)
            .cloned()
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

    /// Atomically validates and upserts a complete provider registration.
    pub fn set_provider(
        &self,
        provider: ProviderRegistration,
    ) -> Result<Option<Arc<ProviderRegistration>>, ProviderRegistrationError> {
        provider.validate()?;
        let provider = Arc::new(provider);
        let previous = write_unpoisoned(&self.inner.providers)
            .insert(provider.descriptor.id.clone(), Arc::clone(&provider));
        Ok(previous)
    }

    /// Atomically removes one provider registration.
    pub fn remove_provider(
        &self,
        provider: &crate::ProviderId,
    ) -> Option<Arc<ProviderRegistration>> {
        write_unpoisoned(&self.inner.providers).shift_remove(provider)
    }

    /// Atomically clears all provider registrations.
    pub fn clear_providers(&self) {
        write_unpoisoned(&self.inner.providers).clear();
    }

    /// Executes the complete simple request pipeline and releases the registry
    /// lock before authentication or any other await point.
    pub fn stream_simple(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<crate::AssistantStream, RequestStartError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| {
                RequestStartError::new(RequestStartErrorKind::Cancelled, "request cancelled")
                    .with_model(request.model.clone())
            })?;

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

            let auth = await_or_cancelled(
                provider.auth.resolve(
                    crate::ResolveAuthRequest {
                        provider: provider.descriptor.clone(),
                        model: model.clone(),
                    },
                    cancellation.clone(),
                ),
                &cancellation,
            )
            .await?
            .map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::RuntimeUnavailable, error.message)
                    .with_model(request.model.clone())
            })?
            .ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::RuntimeUnavailable,
                    format!("provider is not configured: {}", provider.descriptor.id),
                )
                .with_model(request.model.clone())
            })?;

            let endpoint = auth
                .base_url
                .clone()
                .or_else(|| provider.descriptor.base_url.clone())
                .unwrap_or_else(|| model.common.base_url.clone());

            // provider/auth -> model -> explicit request
            let mut headers =
                provider_default_headers(&provider).map_err(AiError::into_request_start)?;
            merge_header_map(&mut headers, &auth.headers);
            apply_header_spec(&mut headers, &model.common.headers).map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                    .with_model(request.model.clone())
            })?;
            apply_header_spec(&mut headers, &request.options.headers).map_err(|error| {
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

            let mut retry_policy = provider.retry_policy.clone();
            if let Some(max_retries) = request.options.max_retries {
                retry_policy.max_retries = max_retries;
            }
            if let Some(max_delay_ms) = request.options.max_retry_delay_ms {
                retry_policy.max_server_delay = Some(Duration::from_millis(max_delay_ms));
            }
            let timeout = request.options.timeout_ms.map(Duration::from_millis);

            implementation
                .stream(
                    ResolvedApiRequest {
                        model,
                        context: request.context,
                        options: request.options,
                        endpoint,
                        headers,
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
#[derive(Default)]
pub struct ModelsBuilder {
    providers: Vec<ProviderRegistration>,
    header_transforms: Vec<Arc<dyn HeaderTransform>>,
    payload_transforms: Vec<Arc<dyn ErasedPayloadTransform>>,
    response_observers: Vec<Arc<dyn ResponseObserver>>,
    attempt_middleware: Vec<Arc<dyn AttemptMiddleware>>,
}

impl ModelsBuilder {
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
        let mut providers = IndexMap::new();
        for provider in self.providers {
            provider.validate()?;
            providers.insert(provider.descriptor.id.clone(), Arc::new(provider));
        }
        Ok(Models {
            inner: Arc::new(ModelsInner {
                providers: RwLock::new(providers),
                header_transforms: Arc::from(self.header_transforms),
                payload_transforms: Arc::from(self.payload_transforms),
                response_observers: Arc::from(self.response_observers),
                attempt_middleware: Arc::from(self.attempt_middleware),
            }),
        })
    }
}

/// Immutable snapshot of currently registered local providers.
pub type LocalProviderSnapshot = Rc<[Rc<LocalProviderRegistration>]>;

/// Immutable flattened local model snapshot.
pub type LocalModelSnapshot = Rc<[ModelDescriptor]>;

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
    providers: RefCell<IndexMap<crate::ProviderId, Rc<LocalProviderRegistration>>>,
    header_transforms: Rc<[Rc<dyn LocalHeaderTransform>]>,
    payload_transforms: Rc<[Rc<dyn LocalErasedPayloadTransform>]>,
    response_observers: Rc<[Rc<dyn LocalResponseObserver>]>,
    attempt_middleware: Rc<[Rc<dyn LocalAttemptMiddleware>]>,
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
                .cloned()
                .collect::<Vec<_>>(),
        )
    }

    /// Returns one registered local provider handle.
    pub fn provider(&self, provider: &crate::ProviderId) -> Option<Rc<LocalProviderRegistration>> {
        self.inner.providers.borrow().get(provider).cloned()
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

    /// Atomically validates and upserts a complete local provider.
    pub fn set_provider(
        &self,
        provider: LocalProviderRegistration,
    ) -> Result<Option<Rc<LocalProviderRegistration>>, ProviderRegistrationError> {
        provider.validate()?;
        let provider = Rc::new(provider);
        Ok(self
            .inner
            .providers
            .borrow_mut()
            .insert(provider.descriptor.id.clone(), Rc::clone(&provider)))
    }

    /// Atomically removes one local provider registration.
    pub fn remove_provider(
        &self,
        provider: &crate::ProviderId,
    ) -> Option<Rc<LocalProviderRegistration>> {
        self.inner.providers.borrow_mut().shift_remove(provider)
    }

    /// Atomically clears all local provider registrations.
    pub fn clear_providers(&self) {
        self.inner.providers.borrow_mut().clear();
    }

    /// Executes the local simple request pipeline without retaining a registry
    /// borrow across authentication or middleware awaits.
    pub fn stream_simple(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| {
                RequestStartError::new(RequestStartErrorKind::Cancelled, "request cancelled")
                    .with_model(request.model.clone())
            })?;

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

            let auth = await_or_cancelled(
                provider.auth.resolve(
                    crate::ResolveAuthRequest {
                        provider: provider.descriptor.clone(),
                        model: model.clone(),
                    },
                    cancellation.clone(),
                ),
                &cancellation,
            )
            .await?
            .map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::RuntimeUnavailable, error.message)
                    .with_model(request.model.clone())
            })?
            .ok_or_else(|| {
                RequestStartError::new(
                    RequestStartErrorKind::RuntimeUnavailable,
                    format!("provider is not configured: {}", provider.descriptor.id),
                )
                .with_model(request.model.clone())
            })?;

            let endpoint = auth
                .base_url
                .clone()
                .or_else(|| provider.descriptor.base_url.clone())
                .unwrap_or_else(|| model.common.base_url.clone());

            let mut headers =
                local_provider_default_headers(&provider).map_err(AiError::into_request_start)?;
            merge_header_map(&mut headers, &auth.headers);
            apply_header_spec(&mut headers, &model.common.headers).map_err(|error| {
                RequestStartError::new(RequestStartErrorKind::InvalidRequest, error.message)
                    .with_model(request.model.clone())
            })?;
            apply_header_spec(&mut headers, &request.options.headers).map_err(|error| {
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

            let mut retry_policy = provider.retry_policy.clone();
            if let Some(max_retries) = request.options.max_retries {
                retry_policy.max_retries = max_retries;
            }
            if let Some(max_delay_ms) = request.options.max_retry_delay_ms {
                retry_policy.max_server_delay = Some(Duration::from_millis(max_delay_ms));
            }

            implementation
                .stream(
                    LocalResolvedApiRequest {
                        model,
                        context: request.context,
                        timeout: request.options.timeout_ms.map(Duration::from_millis),
                        options: request.options,
                        endpoint,
                        headers,
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

/// Immutable local Models configuration builder.
#[derive(Default)]
pub struct LocalModelsBuilder {
    providers: Vec<LocalProviderRegistration>,
    header_transforms: Vec<Rc<dyn LocalHeaderTransform>>,
    payload_transforms: Vec<Rc<dyn LocalErasedPayloadTransform>>,
    response_observers: Vec<Rc<dyn LocalResponseObserver>>,
    attempt_middleware: Vec<Rc<dyn LocalAttemptMiddleware>>,
}

impl LocalModelsBuilder {
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
        let mut providers = IndexMap::new();
        for provider in self.providers {
            provider.validate()?;
            providers.insert(provider.descriptor.id.clone(), Rc::new(provider));
        }
        Ok(LocalModels {
            inner: Rc::new(LocalModelsInner {
                providers: RefCell::new(providers),
                header_transforms: Rc::from(self.header_transforms),
                payload_transforms: Rc::from(self.payload_transforms),
                response_observers: Rc::from(self.response_observers),
                attempt_middleware: Rc::from(self.attempt_middleware),
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

fn read_unpoisoned<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_unpoisoned<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
