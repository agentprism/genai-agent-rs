use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::auth::context::default_provider_auth_context;
use crate::auth::credential_store::InMemoryCredentialStore;
use crate::auth::resolve::{
    AuthResolutionOverrides, ModelsError, ModelsErrorCode, ResolveProviderAuthError,
    resolve_provider_auth,
};
use crate::auth::types::{
    ApiKeyCredential, ApiKeyCredentialType, ApiKeyResolveInput, AuthCheck, AuthContext, AuthError,
    AuthInteraction, AuthOperationOptions, AuthResult, AuthType, Credential, CredentialStore,
    ProviderAuth, ProviderAuthInteraction,
};
use crate::event_stream::{
    AssistantMessageEvent, AssistantMessageEventStream, StreamProtocolError,
};
use crate::models_store::{
    InMemoryModelsStore, ModelsStore, ModelsStoreEntry, ModelsStoreOperationOptions,
};
use crate::types::{
    AbortSignal, Api, AssistantMessage, Context, DeferredCancelOptions, DeferredFetchOptions,
    DeferredHandle, ErrorStopReason, Model, ModelThinkingLevel, ProviderEnv, ProviderHeaders,
    ProviderRequestOptions, SimpleStreamOptions, StopReason, ThinkingLevelMap, Usage, UsageCost,
};
use crate::utils::abort::{
    AbortController, AbortReason, abort_reason, operation_signal, race_with_abort_signal,
};
use crate::utils::abort_signals::combine_abort_signals;
use futures::future::{BoxFuture, join_all};
use futures::{FutureExt, StreamExt};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

pub type ProviderRef = Arc<dyn Provider>;
pub type ModelsFuture<T> = BoxFuture<'static, Result<T, ModelsOperationError>>;
pub type RefreshModelsFuture = BoxFuture<'static, Result<(), ModelsError>>;
pub type FetchModels = Arc<
    dyn Fn(RefreshModelsContext) -> BoxFuture<'static, Result<Vec<Model>, ModelsError>>
        + Send
        + Sync,
>;
pub type FilterModels = Arc<
    dyn for<'a> Fn(Vec<Model>, Option<&'a Credential>) -> Result<Vec<Model>, ModelsError>
        + Send
        + Sync,
>;
pub type TransformHeaders = Arc<
    dyn Fn(ProviderHeaders) -> BoxFuture<'static, Result<ProviderHeaders, ModelsError>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsOperationError {
    Abort(AbortReason),
    Auth(AuthError),
    Models(ModelsError),
}

impl fmt::Display for ModelsOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Abort(error) => error.fmt(formatter),
            Self::Auth(error) => error.fmt(formatter),
            Self::Models(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ModelsOperationError {}

impl From<AbortReason> for ModelsOperationError {
    fn from(value: AbortReason) -> Self {
        Self::Abort(value)
    }
}

impl From<ModelsError> for ModelsOperationError {
    fn from(value: ModelsError) -> Self {
        Self::Models(value)
    }
}

impl From<AuthError> for ModelsOperationError {
    fn from(value: AuthError) -> Self {
        Self::Auth(value)
    }
}

impl From<ResolveProviderAuthError> for ModelsOperationError {
    fn from(value: ResolveProviderAuthError) -> Self {
        match value {
            ResolveProviderAuthError::Abort(error) => Self::Abort(error),
            ResolveProviderAuthError::Models(error) => Self::Models(error),
        }
    }
}

pub enum ModelsPersistence {
    Unchanged,
    Write(ModelsStoreEntry),
    Delete,
}

pub struct ModelsPublication {
    pub persist: ModelsPersistence,
    pub update: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl Default for ModelsPublication {
    fn default() -> Self {
        Self {
            persist: ModelsPersistence::Unchanged,
            update: None,
        }
    }
}

pub type PublishModels =
    Arc<dyn Fn(ModelsPublication) -> BoxFuture<'static, Result<bool, ModelsError>> + Send + Sync>;

#[derive(Clone)]
pub struct RefreshModelsContext {
    pub credential: Option<Credential>,
    pub stored: Option<ModelsStoreEntry>,
    pub publish: PublishModels,
    pub allow_network: bool,
    pub force: Option<bool>,
    pub signal: Arc<dyn AbortSignal>,
}

#[derive(Clone, Default)]
pub struct ModelsRefreshOptions {
    pub allow_network: Option<bool>,
    pub providers: Option<Vec<String>>,
    pub force: Option<bool>,
    pub signal: Option<Arc<dyn AbortSignal>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelsRefreshResult {
    pub aborted: bool,
    pub errors: IndexMap<String, ModelsOperationError>,
}

#[derive(Clone)]
pub struct ModelsApiStreamOptions {
    pub options: ApiStreamOptions,
    pub transform_headers: Option<TransformHeaders>,
}

impl Default for ModelsApiStreamOptions {
    fn default() -> Self {
        Self {
            options: ApiStreamOptions::Base(Default::default()),
            transform_headers: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct ModelsSimpleStreamOptions {
    pub options: SimpleStreamOptions,
    pub transform_headers: Option<TransformHeaders>,
}

#[derive(Clone, Default)]
pub struct ModelsDeferredFetchOptions {
    pub options: DeferredFetchOptions,
    pub transform_headers: Option<TransformHeaders>,
}

#[derive(Clone, Default)]
pub struct ModelsDeferredCancelOptions {
    pub options: DeferredCancelOptions,
    pub transform_headers: Option<TransformHeaders>,
}

pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;

    fn base_url(&self) -> Option<&str> {
        None
    }

    fn headers(&self) -> Option<&ProviderHeaders> {
        None
    }

    fn auth(&self) -> ProviderAuth;
    fn get_models(&self) -> Result<Vec<Model>, ModelsError>;

    fn supports_refresh_models(&self) -> bool {
        false
    }

    fn refresh_models(&self, _context: RefreshModelsContext) -> Option<RefreshModelsFuture> {
        None
    }

    fn filter_models(
        &self,
        models: Vec<Model>,
        _credential: Option<&Credential>,
    ) -> Result<Vec<Model>, ModelsError> {
        Ok(models)
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream;

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream;

    fn supports_fetch_deferred(&self) -> bool {
        false
    }

    fn fetch_deferred(
        &self,
        _model: &Model,
        _handle: &DeferredHandle,
        _options: DeferredFetchOptions,
    ) -> Option<AssistantMessageEventStream> {
        None
    }

    fn supports_cancel_deferred(&self) -> bool {
        false
    }

    fn cancel_deferred<'a>(
        &'a self,
        _model: &'a Model,
        _handle: &'a DeferredHandle,
        _options: DeferredCancelOptions,
    ) -> Option<BoxFuture<'a, Result<(), ModelsError>>> {
        None
    }
}

#[derive(Clone)]
pub enum ProviderApi {
    Single(Arc<dyn ProviderStreams>),
    ByApi(IndexMap<Api, Arc<dyn ProviderStreams>>),
}

pub struct CreateProviderOptions {
    pub id: String,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub auth: ProviderAuth,
    pub models: Vec<Model>,
    pub fetch_models: Option<FetchModels>,
    pub filter_models: Option<FilterModels>,
    pub api: ProviderApi,
}

struct CreatedProvider {
    id: String,
    name: String,
    base_url: Option<String>,
    headers: Option<ProviderHeaders>,
    auth: ProviderAuth,
    baseline_models: Vec<Model>,
    dynamic_models: Arc<RwLock<Vec<Model>>>,
    fetch_models: Option<FetchModels>,
    filter_models: Option<FilterModels>,
    api: ProviderApi,
}

impl CreatedProvider {
    fn api_for(&self, model: &Model) -> Option<&Arc<dyn ProviderStreams>> {
        match &self.api {
            ProviderApi::Single(streams) => Some(streams),
            ProviderApi::ByApi(streams) => streams.get(&model.api),
        }
    }

    fn implementations(&self) -> Vec<&Arc<dyn ProviderStreams>> {
        match &self.api {
            ProviderApi::Single(streams) => vec![streams],
            ProviderApi::ByApi(streams) => streams.values().collect(),
        }
    }
}

impl Provider for CreatedProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    fn headers(&self) -> Option<&ProviderHeaders> {
        self.headers.as_ref()
    }

    fn auth(&self) -> ProviderAuth {
        self.auth.clone()
    }

    fn get_models(&self) -> Result<Vec<Model>, ModelsError> {
        let mut merged = self.baseline_models.clone();
        let dynamic = self
            .dynamic_models
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        for model in dynamic.iter() {
            if let Some(index) = merged.iter().position(|entry| entry.id == model.id) {
                merged[index] = model.clone();
            } else {
                merged.push(model.clone());
            }
        }
        Ok(merged)
    }

    fn refresh_models(&self, context: RefreshModelsContext) -> Option<RefreshModelsFuture> {
        let fetch_models = self.fetch_models.clone()?;
        let provider_id = self.id.clone();
        let dynamic_models = self.dynamic_models.clone();
        Some(Box::pin(async move {
            if let Some(stored) = context.stored.clone() {
                let restored = stored
                    .models
                    .into_iter()
                    .filter(|model| model.provider.as_str() == provider_id)
                    .collect::<Vec<_>>();
                let update_models = dynamic_models.clone();
                if !(context.publish)(ModelsPublication {
                    persist: ModelsPersistence::Unchanged,
                    update: Some(Box::new(move || {
                        *update_models
                            .write()
                            .unwrap_or_else(PoisonError::into_inner) = restored;
                    })),
                })
                .await?
                {
                    return Ok(());
                }
            }
            if !context.allow_network || context.signal.is_aborted() {
                return Ok(());
            }
            let refreshed = fetch_models(context.clone()).await?;
            if context.signal.is_aborted() {
                return Ok(());
            }
            let persisted = refreshed.clone();
            let update_models = dynamic_models;
            (context.publish)(ModelsPublication {
                persist: ModelsPersistence::Write(ModelsStoreEntry {
                    models: persisted,
                    last_modified: None,
                    checked_at: Some(now_ms()),
                    etag: None,
                }),
                update: Some(Box::new(move || {
                    *update_models
                        .write()
                        .unwrap_or_else(PoisonError::into_inner) = refreshed;
                })),
            })
            .await?;
            Ok(())
        }))
    }

    fn supports_refresh_models(&self) -> bool {
        self.fetch_models.is_some()
    }

    fn filter_models(
        &self,
        models: Vec<Model>,
        credential: Option<&Credential>,
    ) -> Result<Vec<Model>, ModelsError> {
        self.filter_models
            .as_ref()
            .map_or(Ok(models.clone()), |filter| filter(models, credential))
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        self.api_for(model).map_or_else(
            || {
                terminal_models_error(
                    model,
                    ModelsError::new(
                        ModelsErrorCode::Stream,
                        format!(
                            "Provider {} has no API implementation for \"{}\"",
                            self.id, model.api
                        ),
                        None,
                    ),
                )
            },
            |streams| streams.stream(model, context, options),
        )
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.api_for(model).map_or_else(
            || {
                terminal_models_error(
                    model,
                    ModelsError::new(
                        ModelsErrorCode::Stream,
                        format!(
                            "Provider {} has no API implementation for \"{}\"",
                            self.id, model.api
                        ),
                        None,
                    ),
                )
            },
            |streams| streams.stream_simple(model, context, options),
        )
    }

    fn supports_fetch_deferred(&self) -> bool {
        self.implementations()
            .into_iter()
            .any(|streams| streams.supports_fetch_deferred())
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> Option<AssistantMessageEventStream> {
        if !self.supports_fetch_deferred() {
            return None;
        }
        Some(
            self.api_for(model)
                .and_then(|streams| streams.fetch_deferred(model, handle, options))
                .unwrap_or_else(|| {
                    terminal_models_error(
                        model,
                        ModelsError::new(
                            ModelsErrorCode::Provider,
                            format!(
                                "Provider {} does not support deferred responses for \"{}\"",
                                self.id, model.api
                            ),
                            None,
                        ),
                    )
                }),
        )
    }

    fn supports_cancel_deferred(&self) -> bool {
        self.implementations()
            .into_iter()
            .any(|streams| streams.supports_cancel_deferred())
    }

    fn cancel_deferred<'a>(
        &'a self,
        model: &'a Model,
        handle: &'a DeferredHandle,
        options: DeferredCancelOptions,
    ) -> Option<BoxFuture<'a, Result<(), ModelsError>>> {
        if !self.supports_cancel_deferred() {
            return None;
        }
        Some(
            match self
                .api_for(model)
                .and_then(|streams| streams.cancel_deferred(model, handle, options))
            {
                Some(cancel) => Box::pin(async move {
                    cancel.await.map_err(|message| {
                        ModelsError::new(
                            ModelsErrorCode::Provider,
                            message
                                .error_message
                                .unwrap_or_else(|| "Deferred cancellation failed".to_owned()),
                            None,
                        )
                    })
                }),
                None => Box::pin(async move {
                    Err(ModelsError::new(
                        ModelsErrorCode::Provider,
                        format!(
                            "Provider {} cannot cancel deferred responses for \"{}\"",
                            self.id, model.api
                        ),
                        None,
                    ))
                }),
            },
        )
    }
}

pub fn create_provider(input: CreateProviderOptions) -> ProviderRef {
    Arc::new(CreatedProvider {
        name: input.name.unwrap_or_else(|| input.id.clone()),
        id: input.id,
        base_url: input.base_url,
        headers: input.headers,
        auth: input.auth,
        baseline_models: input.models,
        dynamic_models: Arc::new(RwLock::new(Vec::new())),
        fetch_models: input.fetch_models,
        filter_models: input.filter_models,
        api: input.api,
    })
}

#[derive(Clone)]
pub struct CreateModelsOptions {
    pub credentials: Arc<dyn CredentialStore>,
    pub models_store: Arc<dyn ModelsStore>,
    pub auth_context: Arc<dyn AuthContext>,
}

impl Default for CreateModelsOptions {
    fn default() -> Self {
        Self {
            credentials: Arc::new(InMemoryCredentialStore::default()),
            models_store: Arc::new(InMemoryModelsStore::default()),
            auth_context: Arc::new(default_provider_auth_context()),
        }
    }
}

struct ModelsInner {
    providers: RwLock<IndexMap<String, ProviderRef>>,
    credentials: Arc<dyn CredentialStore>,
    models_store: Arc<dyn ModelsStore>,
    auth_context: Arc<dyn AuthContext>,
    refresh_generations: Mutex<HashMap<String, u64>>,
    refresh_controllers: Mutex<HashMap<String, AbortController>>,
    publication_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

#[derive(Clone)]
pub struct Models {
    inner: Arc<ModelsInner>,
}

pub type MutableModels = Models;

pub fn create_models(options: Option<CreateModelsOptions>) -> MutableModels {
    let options = options.unwrap_or_default();
    Models {
        inner: Arc::new(ModelsInner {
            providers: RwLock::new(IndexMap::new()),
            credentials: options.credentials,
            models_store: options.models_store,
            auth_context: options.auth_context,
            refresh_generations: Mutex::new(HashMap::new()),
            refresh_controllers: Mutex::new(HashMap::new()),
            publication_locks: Mutex::new(HashMap::new()),
        }),
    }
}

impl Default for Models {
    fn default() -> Self {
        create_models(None)
    }
}

impl Models {
    pub fn set_provider(&self, provider: ProviderRef) {
        self.supersede_provider_refresh(provider.id());
        self.inner
            .providers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(provider.id().to_owned(), provider);
    }

    pub fn delete_provider(&self, id: &str) {
        self.supersede_provider_refresh(id);
        self.inner
            .providers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .shift_remove(id);
    }

    pub fn clear_providers(&self) {
        let mut ids = self
            .inner
            .providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in self
            .inner
            .refresh_controllers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
        {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        for id in ids {
            self.supersede_provider_refresh(&id);
        }
        self.inner
            .providers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    pub fn get_providers(&self) -> Vec<ProviderRef> {
        self.inner
            .providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    pub fn get_provider(&self, id: &str) -> Option<ProviderRef> {
        self.inner
            .providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        if let Some(provider) = provider {
            return self
                .get_provider(provider)
                .and_then(|entry| safe_get_models(&entry).ok())
                .unwrap_or_default();
        }
        self.get_providers()
            .into_iter()
            .flat_map(|provider| safe_get_models(&provider).unwrap_or_default())
            .collect()
    }

    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.get_models(Some(provider))
            .into_iter()
            .find(|model| model.id == id)
    }

    fn supersede_provider_refresh(&self, provider_id: &str) -> u64 {
        let generation = {
            let mut generations = self
                .inner
                .refresh_generations
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let generation = generations.get(provider_id).copied().unwrap_or(0) + 1;
            generations.insert(provider_id.to_owned(), generation);
            generation
        };
        if let Some(previous) = self
            .inner
            .refresh_controllers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(provider_id)
        {
            previous.abort(AbortReason::default_abort());
        }
        generation
    }

    fn begin_provider_refresh(&self, provider_id: &str) -> (u64, AbortController) {
        let generation = self.supersede_provider_refresh(provider_id);
        let controller = AbortController::new();
        self.inner
            .refresh_controllers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(provider_id.to_owned(), controller.clone());
        (generation, controller)
    }

    fn publication_lock(&self, provider_id: &str) -> Arc<AsyncMutex<()>> {
        self.inner
            .publication_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(provider_id.to_owned())
            .or_default()
            .clone()
    }

    fn is_current_generation(&self, provider_id: &str, generation: u64) -> bool {
        self.inner
            .refresh_generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(provider_id)
            .copied()
            == Some(generation)
    }

    fn publish_provider_models(
        &self,
        provider_id: String,
        generation: u64,
        signal: Arc<dyn AbortSignal>,
        mut publication: ModelsPublication,
    ) -> BoxFuture<'static, Result<bool, ModelsError>> {
        let models = self.clone();
        Box::pin(async move {
            let lock = models.publication_lock(&provider_id);
            let operation_signal = signal.clone();
            let operation = async move {
                let _guard = lock.lock().await;
                if operation_signal.is_aborted()
                    || !models.is_current_generation(&provider_id, generation)
                {
                    return Ok(false);
                }
                let store_options = ModelsStoreOperationOptions {
                    signal: Some(operation_signal.clone()),
                };
                match publication.persist {
                    ModelsPersistence::Unchanged => {}
                    ModelsPersistence::Write(entry) => models
                        .inner
                        .models_store
                        .write(&provider_id, entry, store_options)
                        .await
                        .map_err(|error| {
                            ModelsError::new(
                                ModelsErrorCode::ModelSource,
                                format!("Model store write failed for {provider_id}"),
                                Some(&error),
                            )
                        })?,
                    ModelsPersistence::Delete => {
                        models
                            .inner
                            .models_store
                            .delete(&provider_id, store_options)
                            .await
                            .map_err(|error| {
                                ModelsError::new(
                                    ModelsErrorCode::ModelSource,
                                    format!("Model store delete failed for {provider_id}"),
                                    Some(&error),
                                )
                            })?;
                    }
                }
                if operation_signal.is_aborted()
                    || !models.is_current_generation(&provider_id, generation)
                {
                    return Ok(false);
                }
                if let Some(update) = publication.update.take() {
                    update();
                }
                Ok(true)
            };
            match race_with_abort_signal(operation, signal).await {
                Ok(result) => result,
                Err(error) => Err(ModelsError::new(
                    ModelsErrorCode::ModelSource,
                    error.message,
                    None,
                )),
            }
        })
    }

    async fn run_provider_refresh_phase(
        &self,
        provider: ProviderRef,
        credential: Option<Credential>,
        allow_network: bool,
        force: Option<bool>,
        generation: u64,
        signal: Arc<dyn AbortSignal>,
    ) -> Result<(), ModelsError> {
        let stored = self
            .inner
            .models_store
            .read(
                provider.id(),
                ModelsStoreOperationOptions {
                    signal: Some(signal.clone()),
                },
            )
            .await
            .map_err(|error| {
                ModelsError::new(
                    ModelsErrorCode::ModelSource,
                    format!("Model store read failed for {}", provider.id()),
                    Some(&error),
                )
            })?;
        let models = self.clone();
        let provider_id = provider.id().to_owned();
        let publish_signal = signal.clone();
        let context = RefreshModelsContext {
            credential,
            stored,
            publish: Arc::new(move |publication| {
                models.publish_provider_models(
                    provider_id.clone(),
                    generation,
                    publish_signal.clone(),
                    publication,
                )
            }),
            allow_network,
            force: allow_network.then_some(force).flatten(),
            signal,
        };
        match provider.refresh_models(context) {
            Some(refresh) => refresh.await,
            None => Ok(()),
        }
    }

    pub fn refresh(&self, options: ModelsRefreshOptions) -> ModelsFuture<ModelsRefreshResult> {
        let models = self.clone();
        Box::pin(async move {
            let allow_network = options.allow_network.unwrap_or(true);
            let caller_signal = operation_signal(options.signal);
            if caller_signal.is_aborted() {
                return Ok(ModelsRefreshResult {
                    aborted: true,
                    errors: IndexMap::new(),
                });
            }
            let selected = options.providers.map(|providers| {
                providers
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>()
            });
            let refreshable = models
                .get_providers()
                .into_iter()
                .filter(|provider| {
                    provider.supports_refresh_models()
                        && selected
                            .as_ref()
                            .is_none_or(|selected| selected.contains(provider.id()))
                })
                .collect::<Vec<_>>();
            let errors = Arc::new(Mutex::new(IndexMap::new()));
            let operation_models = models.clone();
            let operation_caller_signal = caller_signal.clone();
            let operation_errors = errors.clone();
            let force = options.force;
            let operations = refreshable.into_iter().map(move |provider| {
                let models = operation_models.clone();
                let caller_signal = operation_caller_signal.clone();
                let errors = operation_errors.clone();
                async move {
                    let provider_id = provider.id().to_owned();
                    let (generation, controller) = models.begin_provider_refresh(&provider_id);
                    let combined = combine_abort_signals(&[
                        Some(caller_signal.clone()),
                        Some(controller.signal()),
                    ]);
                    let signal = combined
                        .signal
                        .clone()
                        .unwrap_or_else(|| caller_signal.clone());
                    let operation_models = models.clone();
                    let operation_provider = provider.clone();
                    let operation_signal = signal.clone();
                    let operation = async move {
                        let credential_result = operation_models
                            .read_credential(&provider_id, operation_signal.clone())
                            .await;
                        let stored_credential = credential_result.as_ref().ok().cloned().flatten();
                        operation_models
                            .run_provider_refresh_phase(
                                operation_provider.clone(),
                                stored_credential.clone(),
                                false,
                                None,
                                generation,
                                operation_signal.clone(),
                            )
                            .await
                            .map_err(ModelsOperationError::Models)?;
                        let stored_credential = credential_result?;
                        if !allow_network || operation_signal.is_aborted() {
                            return Ok(());
                        }
                        let credential = operation_models
                            .resolve_refresh_credential(
                                operation_provider.clone(),
                                stored_credential,
                                operation_signal.clone(),
                            )
                            .await?;
                        let Some(credential) = credential else {
                            return Ok(());
                        };
                        operation_models
                            .run_provider_refresh_phase(
                                operation_provider,
                                Some(credential),
                                true,
                                force,
                                generation,
                                operation_signal,
                            )
                            .await
                            .map_err(ModelsOperationError::Models)
                    };
                    let outcome = race_with_abort_signal(operation, signal.clone()).await;
                    if let Ok(Err(error)) = outcome
                        && !signal.is_aborted()
                    {
                        errors
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(provider.id().to_owned(), error);
                    }
                    if models.is_current_generation(provider.id(), generation) {
                        models
                            .inner
                            .refresh_controllers
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(provider.id());
                    }
                    drop(combined);
                }
            });
            let all = async move {
                join_all(operations).await;
            };
            let _ = race_with_abort_signal(all, caller_signal.clone()).await;
            let result_errors = errors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            Ok(ModelsRefreshResult {
                aborted: caller_signal.is_aborted(),
                errors: result_errors,
            })
        })
    }

    async fn resolve_refresh_credential(
        &self,
        provider: ProviderRef,
        stored: Option<Credential>,
        signal: Arc<dyn AbortSignal>,
    ) -> Result<Option<Credential>, ModelsOperationError> {
        if let Some(Credential::OAuth(stored)) = stored {
            let Some(oauth) = provider.auth().oauth else {
                return Ok(None);
            };
            if now_ms() < stored.expires {
                return Ok(Some(Credential::OAuth(stored)));
            }
            if signal.is_aborted() {
                return Ok(None);
            }
            let refresh = oauth.refresh.clone();
            let refresh_signal = signal.clone();
            let post = self
                .inner
                .credentials
                .modify(
                    provider.id().to_owned(),
                    Box::new(move |current| {
                        let refresh = refresh.clone();
                        let refresh_signal = refresh_signal.clone();
                        Box::pin(async move {
                            let Some(Credential::OAuth(current)) = current else {
                                return Ok(None);
                            };
                            if now_ms() < current.expires {
                                return Ok(None);
                            }
                            refresh(current, refresh_signal)
                                .await
                                .map(|credential| Some(Credential::OAuth(credential)))
                        })
                    }),
                    AuthOperationOptions {
                        signal: Some(signal),
                    },
                )
                .await
                .map_err(ModelsOperationError::Auth)?;
            return Ok(post.and_then(|credential| match credential {
                Credential::OAuth(credential) => Some(Credential::OAuth(credential)),
                Credential::ApiKey(_) => None,
            }));
        }

        let Some(api_key) = provider.auth().api_key else {
            return Ok(None);
        };
        let credential = match stored {
            Some(Credential::ApiKey(credential)) => Some(credential),
            _ => None,
        };
        let result = (api_key.resolve)(ApiKeyResolveInput {
            ctx: self.inner.auth_context.clone(),
            credential,
            signal,
        })
        .await
        .map_err(ModelsOperationError::Auth)?;
        Ok(result.map(|result| {
            Credential::ApiKey(ApiKeyCredential {
                kind: ApiKeyCredentialType::ApiKey,
                key: result.auth.api_key,
                env: result.env,
            })
        }))
    }

    async fn read_credential(
        &self,
        provider_id: &str,
        signal: Arc<dyn AbortSignal>,
    ) -> Result<Option<Credential>, ModelsOperationError> {
        self.inner
            .credentials
            .read(
                provider_id.to_owned(),
                AuthOperationOptions {
                    signal: Some(signal),
                },
            )
            .await
            .map_err(|error| {
                ModelsOperationError::Models(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Credential store read failed for {provider_id}"),
                    Some(&error),
                ))
            })
    }

    async fn check_provider_auth(
        &self,
        provider: ProviderRef,
        credential: Option<Credential>,
        signal: Arc<dyn AbortSignal>,
    ) -> Result<Option<AuthCheck>, ModelsOperationError> {
        if matches!(credential, Some(Credential::OAuth(_))) {
            return Ok(provider.auth().oauth.map(|_| AuthCheck {
                source: Some("OAuth".to_owned()),
                kind: AuthType::OAuth,
            }));
        }
        let Some(api_key) = provider.auth().api_key else {
            return Ok(None);
        };
        if let Some(check) = api_key.check {
            return check(ApiKeyResolveInput {
                ctx: self.inner.auth_context.clone(),
                credential: credential.and_then(|credential| match credential {
                    Credential::ApiKey(credential) => Some(credential),
                    Credential::OAuth(_) => None,
                }),
                signal,
            })
            .await
            .map_err(|error| {
                ModelsOperationError::Models(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("API key auth check failed for provider {}", provider.id()),
                    Some(&error),
                ))
            });
        }
        let result = resolve_provider_auth(
            provider.id().to_owned(),
            provider.auth(),
            self.inner.credentials.clone(),
            self.inner.auth_context.clone(),
            AuthResolutionOverrides {
                signal: Some(signal),
                ..Default::default()
            },
        )
        .await
        .map_err(ModelsOperationError::from)?;
        Ok(result.map(|result| AuthCheck {
            source: result.source,
            kind: AuthType::ApiKey,
        }))
    }

    pub fn check_auth(
        &self,
        provider_id: impl Into<String>,
        options: AuthOperationOptions,
    ) -> ModelsFuture<Option<AuthCheck>> {
        let models = self.clone();
        let provider_id = provider_id.into();
        let signal = operation_signal(options.signal);
        let race_signal = signal.clone();
        Box::pin(async move {
            let operation = async move {
                if signal.is_aborted() {
                    return Err(ModelsOperationError::Abort(abort_reason(signal.as_ref())));
                }
                let Some(provider) = models.get_provider(&provider_id) else {
                    return Ok(None);
                };
                let credential = models.read_credential(&provider_id, signal.clone()).await?;
                models
                    .check_provider_auth(provider, credential, signal)
                    .await
            };
            race_with_abort_signal(operation, race_signal)
                .await
                .map_err(ModelsOperationError::Abort)?
        })
    }

    pub fn get_available(
        &self,
        provider_id: Option<String>,
        options: AuthOperationOptions,
    ) -> ModelsFuture<Vec<Model>> {
        let models = self.clone();
        let signal = operation_signal(options.signal);
        let race_signal = signal.clone();
        Box::pin(async move {
            let operation = async move {
                if signal.is_aborted() {
                    return Err(ModelsOperationError::Abort(abort_reason(signal.as_ref())));
                }
                let providers = provider_id.map_or_else(
                    || models.get_providers(),
                    |id| models.get_provider(&id).into_iter().collect(),
                );
                let checks = join_all(providers.into_iter().map(|provider| {
                    let models = models.clone();
                    let signal = signal.clone();
                    async move {
                        let credential = models
                            .read_credential(provider.id(), signal.clone())
                            .await?;
                        let auth = models
                            .check_provider_auth(provider.clone(), credential.clone(), signal)
                            .await?;
                        Ok::<_, ModelsOperationError>((provider, credential, auth))
                    }
                }))
                .await;
                let mut available = Vec::new();
                for check in checks {
                    let (provider, credential, auth) = check?;
                    if auth.is_none() {
                        continue;
                    }
                    let provider_models = safe_get_models(&provider)?;
                    available.extend(provider.filter_models(provider_models, credential.as_ref())?);
                }
                Ok(available)
            };
            race_with_abort_signal(operation, race_signal)
                .await
                .map_err(ModelsOperationError::Abort)?
        })
    }

    pub fn get_auth(
        &self,
        provider_id: impl Into<String>,
        overrides: AuthResolutionOverrides,
    ) -> ModelsFuture<Option<AuthResult>> {
        let provider_id = provider_id.into();
        let models = self.clone();
        Box::pin(async move {
            let Some(provider) = models.get_provider(&provider_id) else {
                return Ok(None);
            };
            resolve_provider_auth(
                provider_id,
                provider.auth(),
                models.inner.credentials.clone(),
                models.inner.auth_context.clone(),
                overrides,
            )
            .await
            .map_err(ModelsOperationError::from)
        })
    }

    pub fn get_model_auth(
        &self,
        model: &Model,
        overrides: AuthResolutionOverrides,
    ) -> ModelsFuture<Option<AuthResult>> {
        let model = model.clone();
        let models = self.clone();
        Box::pin(async move {
            let Some(mut result) = models.get_auth(model.provider.0.clone(), overrides).await?
            else {
                return Ok(None);
            };
            if let Some(headers) = model.headers {
                let model_headers = headers
                    .into_iter()
                    .map(|(name, value)| (name, Some(value)))
                    .collect();
                result.auth.headers = merge_headers(result.auth.headers, Some(model_headers));
            }
            Ok(Some(result))
        })
    }

    pub fn login(
        &self,
        provider_id: impl Into<String>,
        kind: AuthType,
        interaction: Arc<dyn AuthInteraction>,
    ) -> ModelsFuture<Credential> {
        let provider_id = provider_id.into();
        let models = self.clone();
        Box::pin(async move {
            let signal = operation_signal(interaction.signal());
            if signal.is_aborted() {
                return Err(ModelsOperationError::Abort(abort_reason(signal.as_ref())));
            }
            let provider = models.get_provider(&provider_id).ok_or_else(|| {
                ModelsOperationError::Models(ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!("Unknown provider: {provider_id}"),
                    None,
                ))
            })?;
            let auth = provider.auth();
            let provider_interaction = ProviderAuthInteraction {
                interaction,
                signal: signal.clone(),
            };
            let credential = match kind {
                AuthType::OAuth => {
                    let login = auth.oauth.map(|oauth| oauth.login).ok_or_else(|| {
                        ModelsOperationError::Models(ModelsError::new(
                            ModelsErrorCode::Auth,
                            format!("{} does not support oauth login", provider.name()),
                            None,
                        ))
                    })?;
                    Credential::OAuth(
                        race_with_abort_signal(login(provider_interaction), signal.clone())
                            .await
                            .map_err(ModelsOperationError::Abort)?
                            .map_err(ModelsOperationError::Auth)?,
                    )
                }
                AuthType::ApiKey => {
                    let login =
                        auth.api_key
                            .and_then(|api_key| api_key.login)
                            .ok_or_else(|| {
                                ModelsOperationError::Models(ModelsError::new(
                                    ModelsErrorCode::Auth,
                                    format!("{} does not support api_key login", provider.name()),
                                    None,
                                ))
                            })?;
                    Credential::ApiKey(
                        race_with_abort_signal(login(provider_interaction), signal.clone())
                            .await
                            .map_err(ModelsOperationError::Abort)?
                            .map_err(ModelsOperationError::Auth)?,
                    )
                }
            };
            let stored = credential.clone();
            let mutation_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mutation_started_callback = mutation_started.clone();
            let started = Arc::new(tokio::sync::Notify::new());
            let started_callback = started.clone();
            let mut mutation = tokio::spawn(models.inner.credentials.modify(
                provider_id.clone(),
                Box::new(move |_| {
                    mutation_started_callback.store(true, std::sync::atomic::Ordering::SeqCst);
                    started_callback.notify_one();
                    Box::pin(async move { Ok(Some(stored)) })
                }),
                AuthOperationOptions {
                    signal: Some(signal.clone()),
                },
            ));
            let mutation_result = tokio::select! {
                biased;
                _ = signal.cancelled() => {
                    if !mutation_started.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(ModelsOperationError::Abort(abort_reason(signal.as_ref())));
                    }
                    mutation.await.expect("credential store modify task panicked")
                }
                _ = started.notified() => {
                    mutation.await.expect("credential store modify task panicked")
                }
                result = &mut mutation => {
                    result.expect("credential store modify task panicked")
                }
            };
            mutation_result.map_err(|error| {
                if signal.is_aborted() {
                    ModelsOperationError::Abort(abort_reason(signal.as_ref()))
                } else {
                    ModelsOperationError::Models(ModelsError::new(
                        ModelsErrorCode::Auth,
                        format!("Credential store modify failed for {provider_id}"),
                        Some(&error),
                    ))
                }
            })?;
            Ok(credential)
        })
    }

    pub fn logout(
        &self,
        provider_id: impl Into<String>,
        options: AuthOperationOptions,
    ) -> ModelsFuture<()> {
        let provider_id = provider_id.into();
        let credentials = self.inner.credentials.clone();
        Box::pin(async move {
            let signal = operation_signal(options.signal);
            if signal.is_aborted() {
                return Err(ModelsOperationError::Abort(abort_reason(signal.as_ref())));
            }
            credentials
                .delete(
                    provider_id.clone(),
                    AuthOperationOptions {
                        signal: Some(signal.clone()),
                    },
                )
                .await
                .map_err(|error| {
                    if signal.is_aborted() {
                        ModelsOperationError::Abort(abort_reason(signal.as_ref()))
                    } else {
                        ModelsOperationError::Models(ModelsError::new(
                            ModelsErrorCode::Auth,
                            format!("Credential store delete failed for {provider_id}"),
                            Some(&error),
                        ))
                    }
                })
        })
    }

    fn require_provider(&self, model: &Model) -> Result<ProviderRef, ModelsOperationError> {
        self.get_provider(model.provider.as_str()).ok_or_else(|| {
            ModelsOperationError::Models(ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Unknown provider: {}", model.provider),
                None,
            ))
        })
    }

    async fn apply_auth<T: RequestOptionsAccess>(
        &self,
        model: Model,
        mut options: T,
        transform_headers: Option<TransformHeaders>,
    ) -> Result<(Model, T), ModelsOperationError> {
        self.require_provider(&model)?;
        let request = options.request_options();
        let resolution = self
            .get_model_auth(
                &model,
                AuthResolutionOverrides {
                    api_key: request.api_key.clone(),
                    env: request.env.clone(),
                    signal: request.signal.clone(),
                    ..Default::default()
                },
            )
            .await?
            .ok_or_else(|| {
                ModelsOperationError::Models(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Provider is not configured: {}", model.provider),
                    None,
                ))
            })?;
        let explicit = options.request_options().clone();
        let mut headers = merge_headers(resolution.auth.headers, explicit.headers);
        if let Some(transform) = transform_headers {
            headers = Some(
                transform(headers.unwrap_or_default())
                    .await
                    .map_err(ModelsOperationError::Models)?,
            );
        }
        let env = merge_env(resolution.env, explicit.env);
        let mut request_model = model;
        if let Some(base_url) = resolution.auth.base_url {
            request_model.base_url = base_url;
        }
        let request = options.request_options_mut();
        request.api_key = explicit.api_key.or(resolution.auth.api_key);
        request.headers = headers;
        request.env = env;
        Ok((request_model, options))
    }

    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ModelsApiStreamOptions,
    ) -> AssistantMessageEventStream {
        let models = self.clone();
        let model = model.clone();
        let context = context.clone();
        lazy_models_stream(model.clone(), async move {
            let provider = models.require_provider(&model)?;
            let (request_model, request_options) = models
                .apply_auth(model, options.options, options.transform_headers)
                .await?;
            Ok(provider.stream(&request_model, &context, request_options))
        })
    }

    pub async fn complete(
        &self,
        model: &Model,
        context: &Context,
        options: ModelsApiStreamOptions,
    ) -> Result<AssistantMessage, StreamProtocolError> {
        self.stream(model, context, options).result().await
    }

    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: ModelsSimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let models = self.clone();
        let model = model.clone();
        let context = context.clone();
        lazy_models_stream(model.clone(), async move {
            let provider = models.require_provider(&model)?;
            let (request_model, request_options) = models
                .apply_auth(model, options.options, options.transform_headers)
                .await?;
            Ok(provider.stream_simple(&request_model, &context, request_options))
        })
    }

    pub async fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: ModelsSimpleStreamOptions,
    ) -> Result<AssistantMessage, StreamProtocolError> {
        self.stream_simple(model, context, options).result().await
    }

    pub async fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: ModelsDeferredFetchOptions,
    ) -> Result<AssistantMessage, StreamProtocolError> {
        let models = self.clone();
        let model = model.clone();
        let handle = handle.clone();
        lazy_models_stream(model.clone(), async move {
            let provider = models.require_provider(&model)?;
            if !provider.supports_fetch_deferred() {
                return Err(ModelsOperationError::Models(ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!(
                        "Provider {} does not support deferred responses",
                        model.provider
                    ),
                    None,
                )));
            }
            let (request_model, request_options) = models
                .apply_auth(model, options.options, options.transform_headers)
                .await?;
            provider
                .fetch_deferred(&request_model, &handle, request_options)
                .ok_or_else(|| {
                    ModelsOperationError::Models(ModelsError::new(
                        ModelsErrorCode::Provider,
                        format!(
                            "Provider {} does not support deferred responses",
                            request_model.provider
                        ),
                        None,
                    ))
                })
        })
        .result()
        .await
    }

    pub fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: ModelsDeferredCancelOptions,
    ) -> ModelsFuture<()> {
        let models = self.clone();
        let model = model.clone();
        let handle = handle.clone();
        Box::pin(async move {
            let provider = models.require_provider(&model)?;
            if !provider.supports_cancel_deferred() {
                return Err(ModelsOperationError::Models(ModelsError::new(
                    ModelsErrorCode::Provider,
                    format!(
                        "Provider {} does not support deferred responses",
                        model.provider
                    ),
                    None,
                )));
            }
            let (request_model, request_options) = models
                .apply_auth(model, options.options, options.transform_headers)
                .await?;
            provider
                .cancel_deferred(&request_model, &handle, request_options)
                .ok_or_else(|| {
                    ModelsOperationError::Models(ModelsError::new(
                        ModelsErrorCode::Provider,
                        format!(
                            "Provider {} does not support deferred responses",
                            request_model.provider
                        ),
                        None,
                    ))
                })?
                .await
                .map_err(ModelsOperationError::Models)
        })
    }
}

trait RequestOptionsAccess: Clone + Send + 'static {
    fn request_options(&self) -> &ProviderRequestOptions<Model>;
    fn request_options_mut(&mut self) -> &mut ProviderRequestOptions<Model>;
}

impl RequestOptionsAccess for ApiStreamOptions {
    fn request_options(&self) -> &ProviderRequestOptions<Model> {
        match self {
            Self::Base(options) => &options.request,
            Self::AnthropicMessages(options) => &options.stream.request,
            Self::BedrockConverseStream(options) => &options.stream.request,
            Self::OpenAICompletions(options) => &options.stream.request,
            Self::OpenAIResponses(options) => &options.stream.request,
            Self::OpenAICodexResponses(options) => &options.stream.request,
            Self::GoogleGenerativeAI(options) => &options.stream.request,
            Self::GoogleVertex(options) => &options.stream.request,
            Self::Custom { base, .. } => &base.request,
        }
    }

    fn request_options_mut(&mut self) -> &mut ProviderRequestOptions<Model> {
        match self {
            Self::Base(options) => &mut options.request,
            Self::AnthropicMessages(options) => &mut options.stream.request,
            Self::BedrockConverseStream(options) => &mut options.stream.request,
            Self::OpenAICompletions(options) => &mut options.stream.request,
            Self::OpenAIResponses(options) => &mut options.stream.request,
            Self::OpenAICodexResponses(options) => &mut options.stream.request,
            Self::GoogleGenerativeAI(options) => &mut options.stream.request,
            Self::GoogleVertex(options) => &mut options.stream.request,
            Self::Custom { base, .. } => &mut base.request,
        }
    }
}

impl RequestOptionsAccess for SimpleStreamOptions {
    fn request_options(&self) -> &ProviderRequestOptions<Model> {
        &self.stream.request
    }

    fn request_options_mut(&mut self) -> &mut ProviderRequestOptions<Model> {
        &mut self.stream.request
    }
}

impl RequestOptionsAccess for DeferredFetchOptions {
    fn request_options(&self) -> &ProviderRequestOptions<Model> {
        &self.request
    }

    fn request_options_mut(&mut self) -> &mut ProviderRequestOptions<Model> {
        &mut self.request
    }
}

impl RequestOptionsAccess for DeferredCancelOptions {
    fn request_options(&self) -> &ProviderRequestOptions<Model> {
        self
    }

    fn request_options_mut(&mut self) -> &mut ProviderRequestOptions<Model> {
        self
    }
}

fn lazy_models_stream<F>(model: Model, future: F) -> AssistantMessageEventStream
where
    F: std::future::Future<Output = Result<AssistantMessageEventStream, ModelsOperationError>>
        + Send
        + 'static,
{
    let (sender, stream) = AssistantMessageEventStream::channel();
    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(future).catch_unwind().await;
        match outcome {
            Ok(Ok(mut inner)) => {
                while let Some(event) = inner.next().await {
                    if sender.send(event).is_err() {
                        break;
                    }
                }
            }
            Ok(Err(error)) => {
                let _ = sender.send(error_event(&model, error.to_string()));
            }
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|value| (*value).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "Provider operation panicked".to_owned());
                let _ = sender.send(error_event(&model, message));
            }
        }
    });
    stream
}

fn terminal_models_error(model: &Model, error: ModelsError) -> AssistantMessageEventStream {
    AssistantMessageEventStream::from_events(vec![error_event(model, error.message)])
}

fn error_event(model: &Model, message: String) -> AssistantMessageEvent {
    let mut error = AssistantMessage::pending(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_ms() as i64,
    );
    error.stop_reason = StopReason::Error;
    error.error_message = Some(message);
    AssistantMessageEvent::Error {
        reason: ErrorStopReason::Error,
        error,
    }
}

fn safe_get_models(provider: &ProviderRef) -> Result<Vec<Model>, ModelsError> {
    std::panic::catch_unwind(AssertUnwindSafe(|| provider.get_models())).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|value| (*value).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "Provider model source panicked".to_owned());
        Err(ModelsError::new(
            ModelsErrorCode::ModelSource,
            message,
            None,
        ))
    })
}

fn merge_env(base: Option<ProviderEnv>, overrides: Option<ProviderEnv>) -> Option<ProviderEnv> {
    if base.is_none() && overrides.is_none() {
        return None;
    }
    let mut merged = base.unwrap_or_default();
    if let Some(overrides) = overrides {
        merged.extend(overrides);
    }
    Some(merged)
}

fn merge_headers(
    base: Option<ProviderHeaders>,
    overrides: Option<ProviderHeaders>,
) -> Option<ProviderHeaders> {
    if base.is_none() && overrides.is_none() {
        return None;
    }
    let mut merged = base.unwrap_or_default();
    for (name, value) in overrides.unwrap_or_default() {
        let lower_name = name.to_lowercase();
        merged.retain(|existing, _| existing.to_lowercase() != lower_name);
        merged.insert(name, value);
    }
    Some(merged)
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1_000.0
}

const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

pub fn calculate_cost<'a>(model: &Model, usage: &'a mut Usage) -> &'a UsageCost {
    let input_tokens = usage
        .input
        .js_add(&usage.cache_read)
        .js_add(&usage.cache_write)
        .as_number();
    let mut rates = &model.cost.rates;
    let mut matched_threshold = None;
    for tier in model.cost.tiers.iter().flatten() {
        let threshold = tier.input_tokens_above as f64;
        if input_tokens > threshold && matched_threshold.is_none_or(|matched| threshold > matched) {
            rates = &tier.rates;
            matched_threshold = Some(threshold);
        }
    }

    let long_write = usage
        .cache_write_1h
        .as_ref()
        .map_or(0.0, crate::types::UsageValue::as_number);
    let short_write = usage.cache_write.as_number() - long_write;
    usage.cost.input = rates.input / 1_000_000.0 * usage.input.as_number();
    usage.cost.output = rates.output / 1_000_000.0 * usage.output.as_number();
    usage.cost.cache_read = rates.cache_read / 1_000_000.0 * usage.cache_read.as_number();
    usage.cost.cache_write =
        (rates.cache_write * short_write + rates.input * 2.0 * long_write) / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    &usage.cost
}

fn mapped_level(
    map: Option<&ThinkingLevelMap>,
    level: ModelThinkingLevel,
) -> Option<&Option<String>> {
    let map = map?;
    match level {
        ModelThinkingLevel::Off => map.off.as_ref(),
        ModelThinkingLevel::Minimal => map.minimal.as_ref(),
        ModelThinkingLevel::Low => map.low.as_ref(),
        ModelThinkingLevel::Medium => map.medium.as_ref(),
        ModelThinkingLevel::High => map.high.as_ref(),
        ModelThinkingLevel::Xhigh => map.xhigh.as_ref(),
        ModelThinkingLevel::Max => map.max.as_ref(),
    }
}

pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    EXTENDED_THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let mapped = mapped_level(model.thinking_level_map.as_ref(), *level);
            if mapped == Some(&None) {
                return false;
            }
            !matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max)
                || mapped.is_some()
        })
        .collect()
}

pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = get_supported_thinking_levels(model);
    if available.contains(&level) {
        return level;
    }
    let requested = EXTENDED_THINKING_LEVELS
        .iter()
        .position(|candidate| *candidate == level);
    let Some(requested) = requested else {
        return available
            .first()
            .copied()
            .unwrap_or(ModelThinkingLevel::Off);
    };
    for candidate in &EXTENDED_THINKING_LEVELS[requested..] {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested].iter().rev() {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    available
        .first()
        .copied()
        .unwrap_or(ModelThinkingLevel::Off)
}

pub fn models_are_equal(a: Option<&Model>, b: Option<&Model>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a.id == b.id && a.provider == b.provider)
}

pub fn has_api(model: &Model, api: &Api) -> bool {
    &model.api == api
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::credential_store::InMemoryCredentialStore;
    use crate::auth::types::{
        ApiKeyAuth, ApiKeyCredential, ApiKeyCredentialType, AuthError, AuthEvent, AuthFuture,
        AuthInteraction, AuthPrompt, Credential, ModelAuth, OAuthAuth, OAuthCredential,
        OAuthCredentialType,
    };
    use crate::event_stream::AssistantMessageEvent;
    use crate::models_store::InMemoryModelsStore;
    use crate::providers::faux::{RegisterFauxProviderOptions, create_faux_core, faux_provider};
    use crate::types::{
        Api, ModelCost, ModelCostRates, ModelCostTier, ModelInput, ProviderId, StopReason,
        StreamOptions, SuccessfulStopReason,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    type RuntimeModelSource = Arc<dyn Fn() -> Result<Vec<Model>, ModelsError> + Send + Sync>;
    type RuntimeRefresh = Arc<dyn Fn(RefreshModelsContext) -> RefreshModelsFuture + Send + Sync>;

    struct RuntimeProvider {
        id: String,
        auth: ProviderAuth,
        models: RuntimeModelSource,
        refresh: Option<RuntimeRefresh>,
        streams: Arc<dyn ProviderStreams>,
    }

    impl Provider for RuntimeProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn name(&self) -> &str {
            &self.id
        }

        fn auth(&self) -> ProviderAuth {
            self.auth.clone()
        }

        fn get_models(&self) -> Result<Vec<Model>, ModelsError> {
            (self.models)()
        }

        fn supports_refresh_models(&self) -> bool {
            self.refresh.is_some()
        }

        fn refresh_models(&self, context: RefreshModelsContext) -> Option<RefreshModelsFuture> {
            self.refresh.as_ref().map(|refresh| refresh(context))
        }

        fn stream(
            &self,
            model: &Model,
            context: &Context,
            options: ApiStreamOptions,
        ) -> AssistantMessageEventStream {
            self.streams.stream(model, context, options)
        }

        fn stream_simple(
            &self,
            model: &Model,
            context: &Context,
            options: SimpleStreamOptions,
        ) -> AssistantMessageEventStream {
            self.streams.stream_simple(model, context, options)
        }
    }

    fn runtime_provider(
        id: &str,
        auth: ProviderAuth,
        models: RuntimeModelSource,
        refresh: Option<RuntimeRefresh>,
    ) -> ProviderRef {
        Arc::new(RuntimeProvider {
            id: id.to_owned(),
            auth,
            models,
            refresh,
            streams: Arc::new(create_faux_core(RegisterFauxProviderOptions::default())),
        })
    }

    fn oauth_auth(
        refresh: crate::auth::types::OAuthRefresh,
        to_auth: crate::auth::types::OAuthToAuth,
    ) -> OAuthAuth {
        OAuthAuth {
            name: "Test OAuth".to_owned(),
            is_subscription: None,
            login_label: None,
            login: Arc::new(|_| Box::pin(async { Err(AuthError::new("not used")) })),
            refresh,
            to_auth,
        }
    }

    fn oauth_credential(access: &str, expires: f64) -> Credential {
        Credential::OAuth(OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "refresh".to_owned(),
            access: access.to_owned(),
            expires,
            extra: Default::default(),
        })
    }

    #[derive(Clone)]
    struct FixedInteraction {
        signal: Option<Arc<dyn AbortSignal>>,
        answer: String,
    }

    impl AuthInteraction for FixedInteraction {
        fn signal(&self) -> Option<Arc<dyn AbortSignal>> {
            self.signal.clone()
        }

        fn prompt(&self, _prompt: AuthPrompt) -> AuthFuture<String> {
            let answer = self.answer.clone();
            Box::pin(async move { Ok(answer) })
        }

        fn notify(&self, _event: AuthEvent) {}
    }

    #[derive(Default)]
    struct CountingModelsStore {
        inner: InMemoryModelsStore,
        deletes: AtomicUsize,
    }

    impl ModelsStore for CountingModelsStore {
        fn read<'a>(
            &'a self,
            provider_id: &'a str,
            options: ModelsStoreOperationOptions,
        ) -> BoxFuture<'a, Result<Option<ModelsStoreEntry>, AbortReason>> {
            self.inner.read(provider_id, options)
        }

        fn write<'a>(
            &'a self,
            provider_id: &'a str,
            entry: ModelsStoreEntry,
            options: ModelsStoreOperationOptions,
        ) -> BoxFuture<'a, Result<(), AbortReason>> {
            self.inner.write(provider_id, entry, options)
        }

        fn delete<'a>(
            &'a self,
            provider_id: &'a str,
            options: ModelsStoreOperationOptions,
        ) -> BoxFuture<'a, Result<(), AbortReason>> {
            self.deletes.fetch_add(1, Ordering::SeqCst);
            self.inner.delete(provider_id, options)
        }
    }

    #[derive(Default)]
    struct PointOfNoReturnCredentialStore {
        credential: Arc<Mutex<Option<Credential>>>,
        callback_returned: Arc<Notify>,
        release_mutation: Arc<Notify>,
    }

    impl CredentialStore for PointOfNoReturnCredentialStore {
        fn read(
            &self,
            _provider_id: String,
            _options: AuthOperationOptions,
        ) -> AuthFuture<Option<Credential>> {
            let credential = self
                .credential
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            Box::pin(async move { Ok(credential) })
        }

        fn list(
            &self,
            _options: AuthOperationOptions,
        ) -> AuthFuture<Vec<crate::auth::types::CredentialInfo>> {
            let kind = self
                .credential
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(Credential::auth_type);
            Box::pin(async move {
                Ok(kind.map_or_else(Vec::new, |kind| {
                    vec![crate::auth::types::CredentialInfo {
                        provider_id: "p1".to_owned(),
                        kind,
                    }]
                }))
            })
        }

        fn modify(
            &self,
            _provider_id: String,
            modify: crate::auth::types::CredentialModify,
            _options: AuthOperationOptions,
        ) -> AuthFuture<Option<Credential>> {
            let slot = self.credential.clone();
            let callback_returned = self.callback_returned.clone();
            let release_mutation = self.release_mutation.clone();
            Box::pin(async move {
                let current = slot.lock().unwrap_or_else(PoisonError::into_inner).clone();
                let next = modify(current.clone()).await?;
                callback_returned.notify_one();
                release_mutation.notified().await;
                if let Some(next) = next.clone() {
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(next);
                }
                Ok(next.or(current))
            })
        }

        fn delete(&self, _provider_id: String, _options: AuthOperationOptions) -> AuthFuture<()> {
            let slot = self.credential.clone();
            Box::pin(async move {
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
                Ok(())
            })
        }
    }

    struct RejectingModifyCredentialStore {
        credential: Credential,
        error: AuthError,
    }

    impl CredentialStore for RejectingModifyCredentialStore {
        fn read(
            &self,
            _provider_id: String,
            _options: AuthOperationOptions,
        ) -> AuthFuture<Option<Credential>> {
            let credential = self.credential.clone();
            Box::pin(async move { Ok(Some(credential)) })
        }

        fn list(
            &self,
            _options: AuthOperationOptions,
        ) -> AuthFuture<Vec<crate::auth::types::CredentialInfo>> {
            let kind = self.credential.auth_type();
            Box::pin(async move {
                Ok(vec![crate::auth::types::CredentialInfo {
                    provider_id: "oauth-store".to_owned(),
                    kind,
                }])
            })
        }

        fn modify(
            &self,
            _provider_id: String,
            _modify: crate::auth::types::CredentialModify,
            _options: AuthOperationOptions,
        ) -> AuthFuture<Option<Credential>> {
            let error = self.error.clone();
            Box::pin(async move { Err(error) })
        }

        fn delete(&self, _provider_id: String, _options: AuthOperationOptions) -> AuthFuture<()> {
            Box::pin(async { Ok(()) })
        }
    }

    fn test_model(provider: &str, id: &str) -> Model {
        Model {
            id: id.to_owned(),
            name: id.to_owned(),
            api: "test-api".into(),
            provider: provider.into(),
            base_url: "https://example.test/v1".to_owned(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 10_000,
            max_tokens: 1_000,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn ambient_auth() -> ProviderAuth {
        ProviderAuth {
            api_key: Some(ApiKeyAuth {
                name: "Ambient".to_owned(),
                login: None,
                check: None,
                resolve: Arc::new(|_| {
                    Box::pin(async {
                        Ok(Some(AuthResult {
                            auth: ModelAuth::default(),
                            env: None,
                            source: None,
                        }))
                    })
                }),
            }),
            oauth: None,
        }
    }

    fn test_provider(id: &str, models: Vec<Model>) -> ProviderRef {
        let core = create_faux_core(RegisterFauxProviderOptions::default());
        create_provider(CreateProviderOptions {
            id: id.to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ambient_auth(),
            models,
            fetch_models: None,
            filter_models: None,
            api: ProviderApi::Single(Arc::new(core)),
        })
    }

    fn dynamic_provider(id: &str, fetch_models: FetchModels) -> ProviderRef {
        let core = create_faux_core(RegisterFauxProviderOptions::default());
        create_provider(CreateProviderOptions {
            id: id.to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ambient_auth(),
            models: Vec::new(),
            fetch_models: Some(fetch_models),
            filter_models: None,
            api: ProviderApi::Single(Arc::new(core)),
        })
    }

    #[derive(Clone)]
    struct CaptureStreams {
        calls: Arc<Mutex<Vec<(Model, SimpleStreamOptions)>>>,
    }

    impl CaptureStreams {
        fn done(model: &Model) -> AssistantMessageEventStream {
            let mut message = AssistantMessage::pending(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                0,
            );
            message.stop_reason = StopReason::Stop;
            AssistantMessageEventStream::from_events(vec![
                AssistantMessageEvent::Start,
                AssistantMessageEvent::Done {
                    reason: SuccessfulStopReason::Stop,
                    message,
                },
            ])
        }
    }

    impl ProviderStreams for CaptureStreams {
        fn stream(
            &self,
            model: &Model,
            _context: &Context,
            options: ApiStreamOptions,
        ) -> AssistantMessageEventStream {
            let options = match options {
                ApiStreamOptions::Base(options) => SimpleStreamOptions {
                    stream: options,
                    ..Default::default()
                },
                _ => SimpleStreamOptions::default(),
            };
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((model.clone(), options));
            Self::done(model)
        }

        fn stream_simple(
            &self,
            model: &Model,
            _context: &Context,
            options: SimpleStreamOptions,
        ) -> AssistantMessageEventStream {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((model.clone(), options));
            Self::done(model)
        }
    }

    /// Ports pi `test/models-runtime.test.ts:126-160`.
    #[test]
    fn request_wide_pricing_tiers_use_total_input() {
        let mut model = test_model("openai", "gpt-5.6-sol");
        model.cost = ModelCost {
            rates: ModelCostRates {
                input: 5.0,
                output: 30.0,
                cache_read: 0.5,
                cache_write: 6.25,
            },
            tiers: Some(vec![ModelCostTier {
                input_tokens_above: 272_000,
                rates: ModelCostRates {
                    input: 10.0,
                    output: 45.0,
                    cache_read: 1.0,
                    cache_write: 12.5,
                },
            }]),
        };
        let usage = |cache_write: u64| Usage {
            input: 200_000.into(),
            output: 100_000.into(),
            cache_read: 72_000.into(),
            cache_write: cache_write.into(),
            total_tokens: (372_000 + cache_write).into(),
            ..Default::default()
        };
        let mut short = usage(0);
        let short = calculate_cost(&model, &mut short);
        assert_eq!(
            (short.input, short.output, short.cache_read),
            (1.0, 3.0, 0.036)
        );
        let mut long = usage(1);
        let long = calculate_cost(&model, &mut long);
        assert_eq!(
            (long.input, long.output, long.cache_read),
            (2.0, 4.5, 0.072)
        );
        assert_eq!(long.cache_write, 0.0000125);
    }

    /// Ports pi `test/models-runtime.test.ts:162-199`.
    #[test]
    fn providers_are_ordered_replaceable_and_models_are_searchable() {
        let models = create_models(None);
        models.set_provider(test_provider(
            "p1",
            vec![test_model("p1", "m1"), test_model("p1", "m2")],
        ));
        models.set_provider(test_provider("p2", vec![test_model("p2", "m3")]));
        assert_eq!(
            models
                .get_providers()
                .iter()
                .map(|provider| provider.id())
                .collect::<Vec<_>>(),
            ["p1", "p2"]
        );
        models.set_provider(test_provider("p1", vec![test_model("p1", "replacement")]));
        assert_eq!(models.get_providers().len(), 2);
        assert_eq!(
            models
                .get_models(None)
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["replacement", "m3"]
        );
        let found = models.get_model("p2", "m3").expect("model");
        assert!(has_api(&found, &Api::from("test-api")));
        assert!(!has_api(&found, &Api::from("openai-completions")));
        models.delete_provider("p1");
        assert!(models.get_provider("p1").is_none());
        models.clear_providers();
        assert!(models.get_providers().is_empty());
    }

    /// Ports pi `test/models-runtime.test.ts:219-278,365-425`.
    #[tokio::test]
    async fn refresh_selects_configured_dynamic_providers_persists_and_reports_failures() {
        let store = Arc::new(CountingModelsStore::default());
        let models = create_models(Some(CreateModelsOptions {
            models_store: store.clone(),
            ..Default::default()
        }));
        let fetched = Arc::new(AtomicUsize::new(0));
        let fetched_calls = fetched.clone();
        models.set_provider(dynamic_provider(
            "dynamic",
            Arc::new(move |context| {
                let fetched_calls = fetched_calls.clone();
                Box::pin(async move {
                    assert!(context.allow_network);
                    assert_eq!(context.force, Some(true));
                    fetched_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![test_model("dynamic", "fetched")])
                })
            }),
        ));
        models.set_provider(dynamic_provider(
            "flaky",
            Arc::new(|_| {
                Box::pin(async {
                    Err(ModelsError::new(
                        ModelsErrorCode::ModelSource,
                        "fetch failed",
                        None,
                    ))
                })
            }),
        ));
        let result = models
            .refresh(ModelsRefreshOptions {
                providers: Some(vec!["dynamic".to_owned(), "unknown".to_owned()]),
                force: Some(true),
                ..Default::default()
            })
            .await
            .expect("refresh");
        assert!(!result.aborted);
        assert!(result.errors.is_empty());
        assert_eq!(fetched.load(Ordering::SeqCst), 1);
        assert!(models.get_model("dynamic", "fetched").is_some());
        assert!(
            store
                .read("dynamic", ModelsStoreOperationOptions::default())
                .await
                .expect("stored catalog")
                .is_some()
        );

        let result = models
            .refresh(ModelsRefreshOptions {
                providers: Some(vec!["flaky".to_owned()]),
                ..Default::default()
            })
            .await
            .expect("refresh errors remain in-band");
        assert!(matches!(
            &result.errors["flaky"],
            ModelsOperationError::Models(error) if error.message == "fetch failed"
        ));

        let offline = create_models(Some(CreateModelsOptions {
            models_store: store,
            ..Default::default()
        }));
        offline.set_provider(dynamic_provider(
            "dynamic",
            Arc::new(|_| panic!("offline refresh must not fetch")),
        ));
        let restored = offline
            .refresh(ModelsRefreshOptions {
                allow_network: Some(false),
                ..Default::default()
            })
            .await
            .expect("offline restore");
        assert!(restored.errors.is_empty());
        assert!(offline.get_model("dynamic", "fetched").is_some());
    }

    /// Ports pi `test/models-runtime.test.ts:1052-1135`.
    #[tokio::test]
    async fn request_auth_merging_uses_explicit_values_and_transforms_headers_last() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let streams = CaptureStreams {
            calls: calls.clone(),
        };
        let auth = ProviderAuth {
            api_key: Some(ApiKeyAuth {
                name: "Scoped".to_owned(),
                login: None,
                check: None,
                resolve: Arc::new(|input| {
                    Box::pin(async move {
                        let account = input
                            .credential
                            .as_ref()
                            .and_then(|credential| credential.env.as_ref())
                            .and_then(|env| env.get("ACCOUNT_ID"))
                            .cloned();
                        Ok(Some(AuthResult {
                            auth: ModelAuth {
                                api_key: Some("resolved-key".to_owned()),
                                headers: Some(ProviderHeaders::from([
                                    (
                                        "Authorization".to_owned(),
                                        Some("Bearer resolved".to_owned()),
                                    ),
                                    ("x-a".to_owned(), Some("auth".to_owned())),
                                    ("x-shared".to_owned(), Some("auth".to_owned())),
                                ])),
                                base_url: Some(format!(
                                    "https://auth.test/{}",
                                    account.as_deref().unwrap_or("default")
                                )),
                            },
                            env: account.map(|account| {
                                ProviderEnv::from([("ACCOUNT_ID".to_owned(), account)])
                            }),
                            source: Some("test".to_owned()),
                        }))
                    })
                }),
            }),
            oauth: None,
        };
        let mut model = test_model("p1", "model-a");
        model.headers = Some(IndexMap::from([
            ("x-model".to_owned(), "model".to_owned()),
            ("x-shared".to_owned(), "model".to_owned()),
        ]));
        let provider = create_provider(CreateProviderOptions {
            id: "p1".to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth,
            models: vec![model.clone()],
            fetch_models: None,
            filter_models: None,
            api: ProviderApi::Single(Arc::new(streams)),
        });
        let models = create_models(None);
        models.set_provider(provider);

        let provider_auth = models
            .get_auth("p1", AuthResolutionOverrides::default())
            .await
            .expect("get auth")
            .expect("configured");
        assert!(
            provider_auth
                .auth
                .headers
                .as_ref()
                .is_some_and(|headers| !headers.contains_key("x-model"))
        );
        let model_auth = models
            .get_model_auth(&model, AuthResolutionOverrides::default())
            .await
            .expect("get model auth")
            .expect("configured");
        assert_eq!(
            model_auth
                .auth
                .headers
                .as_ref()
                .and_then(|headers| headers.get("x-model")),
            Some(&Some("model".to_owned()))
        );

        let transformed = Arc::new(|mut headers: ProviderHeaders| {
            Box::pin(async move {
                headers.insert("x-transformed".to_owned(), Some("yes".to_owned()));
                Ok(headers)
            }) as BoxFuture<'static, Result<ProviderHeaders, ModelsError>>
        });
        let options = ModelsSimpleStreamOptions {
            options: SimpleStreamOptions {
                stream: StreamOptions {
                    request: ProviderRequestOptions {
                        api_key: Some("explicit-key".to_owned()),
                        env: Some(ProviderEnv::from([(
                            "ACCOUNT_ID".to_owned(),
                            "acct".to_owned(),
                        )])),
                        headers: Some(ProviderHeaders::from([
                            (
                                "authorization".to_owned(),
                                Some("Explicit token".to_owned()),
                            ),
                            ("X-Shared".to_owned(), Some("explicit".to_owned())),
                        ])),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            transform_headers: Some(transformed),
        };
        let result = models
            .complete_simple(&model, &Context::default(), options)
            .await
            .expect("complete");
        assert_eq!(result.stop_reason, StopReason::Stop);
        let calls = calls.lock().unwrap_or_else(PoisonError::into_inner);
        let (request_model, options) = &calls[0];
        assert_eq!(request_model.base_url, "https://auth.test/acct");
        assert_eq!(
            options.stream.request.api_key.as_deref(),
            Some("explicit-key")
        );
        let headers = options.stream.request.headers.as_ref().expect("headers");
        assert_eq!(
            headers.get("authorization"),
            Some(&Some("Explicit token".to_owned()))
        );
        assert_eq!(headers.get("x-a"), Some(&Some("auth".to_owned())));
        assert_eq!(headers.get("X-Shared"), Some(&Some("explicit".to_owned())));
        assert_eq!(headers.get("x-model"), Some(&Some("model".to_owned())));
        assert_eq!(headers.get("x-transformed"), Some(&Some("yes".to_owned())));
    }

    /// Ports pi `test/models-runtime.test.ts:1138-1158`.
    #[tokio::test]
    async fn unknown_provider_is_an_error_stream_and_known_provider_streams() {
        let models = create_models(None);
        let unknown = models
            .complete_simple(
                &test_model("ghost", "model-a"),
                &Context::default(),
                ModelsSimpleStreamOptions::default(),
            )
            .await
            .expect("terminal error message");
        assert_eq!(unknown.stop_reason, StopReason::Error);
        assert!(
            unknown
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("Unknown provider: ghost"))
        );

        let faux = faux_provider(RegisterFauxProviderOptions::default());
        let model = faux.models[0].clone();
        faux.set_responses(vec![
            crate::providers::faux::faux_assistant_message("ok", Default::default()).into(),
        ]);
        models.set_provider(faux.provider);
        let result = models
            .complete_simple(
                &model,
                &Context::default(),
                ModelsSimpleStreamOptions::default(),
            )
            .await
            .expect("complete");
        assert_eq!(result.stop_reason, StopReason::Stop);
    }

    /// Ports pi `test/models-runtime.test.ts:201-217`.
    #[test]
    fn provider_model_source_failures_are_swallowed_only_by_collection_lookups() {
        let models = create_models(None);
        models.set_provider(runtime_provider(
            "broken",
            ambient_auth(),
            Arc::new(|| panic!("boom")),
            None,
        ));
        models.set_provider(test_provider("ok", vec![test_model("ok", "m1")]));
        assert_eq!(
            models
                .get_models(None)
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["m1"]
        );
        assert!(models.get_models(Some("broken")).is_empty());
        let provider = models
            .get_provider("broken")
            .expect("provider remains registered");
        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| provider.get_models())).is_err());
    }

    /// Ports pi `test/models-runtime.test.ts:324-363,462-513,570-615`.
    #[tokio::test]
    async fn refresh_publication_is_atomic_signal_bound_and_rejects_superseded_work() {
        let store = Arc::new(CountingModelsStore::default());
        store
            .write(
                "published",
                ModelsStoreEntry {
                    models: vec![test_model("published", "stored")],
                    last_modified: None,
                    checked_at: None,
                    etag: None,
                },
                ModelsStoreOperationOptions::default(),
            )
            .await
            .expect("seed store");
        let state = Arc::new(Mutex::new("initial".to_owned()));
        let publication_state = state.clone();
        let refresh: RuntimeRefresh = Arc::new(move |context| {
            let publication_state = publication_state.clone();
            Box::pin(async move {
                assert!(!context.signal.is_aborted());
                assert_eq!(
                    context.stored.as_ref().expect("stored").models[0].id,
                    "stored"
                );
                let deleted_state = publication_state.clone();
                assert!(
                    (context.publish)(ModelsPublication {
                        persist: ModelsPersistence::Delete,
                        update: Some(Box::new(move || {
                            *deleted_state.lock().unwrap_or_else(PoisonError::into_inner) =
                                "deleted".to_owned();
                        })),
                    })
                    .await?
                );
                let ephemeral_state = publication_state.clone();
                assert!(
                    (context.publish)(ModelsPublication {
                        persist: ModelsPersistence::Unchanged,
                        update: Some(Box::new(move || {
                            *ephemeral_state
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner) = "ephemeral".to_owned();
                        })),
                    })
                    .await?
                );
                Ok(())
            })
        });
        let published = create_models(Some(CreateModelsOptions {
            models_store: store.clone(),
            ..Default::default()
        }));
        published.set_provider(runtime_provider(
            "published",
            ambient_auth(),
            Arc::new(|| Ok(vec![test_model("published", "baseline")])),
            Some(refresh),
        ));
        let result = published
            .refresh(ModelsRefreshOptions {
                allow_network: Some(false),
                ..Default::default()
            })
            .await
            .expect("refresh");
        assert!(result.errors.is_empty());
        assert!(
            store
                .read("published", ModelsStoreOperationOptions::default())
                .await
                .expect("read")
                .is_none()
        );
        assert_eq!(store.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(
            *state.lock().unwrap_or_else(PoisonError::into_inner),
            "ephemeral"
        );

        let first_started = Arc::new(Notify::new());
        let finish_first = Arc::new(Notify::new());
        let fetches = Arc::new(AtomicUsize::new(0));
        let fetch_started = first_started.clone();
        let fetch_finish = finish_first.clone();
        let fetch_count = fetches.clone();
        let provider = dynamic_provider(
            "dynamic",
            Arc::new(move |_| {
                let fetch_started = fetch_started.clone();
                let fetch_finish = fetch_finish.clone();
                let fetch_count = fetch_count.clone();
                Box::pin(async move {
                    let current = fetch_count.fetch_add(1, Ordering::SeqCst) + 1;
                    if current == 1 {
                        fetch_started.notify_one();
                        fetch_finish.notified().await;
                    }
                    Ok(vec![test_model(
                        "dynamic",
                        &format!("generation-{current}"),
                    )])
                })
            }),
        );
        let superseded_store = Arc::new(InMemoryModelsStore::default());
        let superseded = create_models(Some(CreateModelsOptions {
            models_store: superseded_store.clone(),
            ..Default::default()
        }));
        superseded.set_provider(provider);
        let first = tokio::spawn(superseded.refresh(ModelsRefreshOptions::default()));
        first_started.notified().await;
        let second = superseded.refresh(ModelsRefreshOptions::default());
        let second_result = second.await.expect("newer refresh");
        assert!(!second_result.aborted);
        assert!(second_result.errors.is_empty());
        assert!(
            first
                .await
                .expect("refresh task")
                .expect("superseded refresh")
                .errors
                .is_empty()
        );
        finish_first.notify_waiters();
        tokio::task::yield_now().await;
        assert!(superseded.get_model("dynamic", "generation-2").is_some());
        assert!(superseded.get_model("dynamic", "generation-1").is_none());
        assert_eq!(
            superseded_store
                .read("dynamic", ModelsStoreOperationOptions::default())
                .await
                .expect("stored")
                .expect("catalog")
                .models[0]
                .id,
            "generation-2"
        );
    }

    /// Ports pi `test/models-runtime.test.ts:280-322,515-568,649-702`.
    #[tokio::test]
    async fn refresh_and_auth_stop_waiting_for_non_cooperative_callbacks() {
        let store = Arc::new(InMemoryModelsStore::default());
        store
            .write(
                "dynamic",
                ModelsStoreEntry {
                    models: vec![test_model("dynamic", "cached")],
                    last_modified: None,
                    checked_at: None,
                    etag: None,
                },
                ModelsStoreOperationOptions::default(),
            )
            .await
            .expect("seed");
        let auth_started = Arc::new(Notify::new());
        let auth_started_callback = auth_started.clone();
        let blocked_auth = ApiKeyAuth {
            name: "Blocked".to_owned(),
            login: None,
            check: None,
            resolve: Arc::new(move |_| {
                let started = auth_started_callback.clone();
                Box::pin(async move {
                    started.notify_one();
                    futures::future::pending::<Result<Option<AuthResult>, AuthError>>().await
                })
            }),
        };
        let provider = create_provider(CreateProviderOptions {
            id: "dynamic".to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ProviderAuth {
                api_key: Some(blocked_auth),
                oauth: None,
            },
            models: Vec::new(),
            fetch_models: Some(Arc::new(|_| panic!("network fetch must not start"))),
            filter_models: None,
            api: ProviderApi::Single(Arc::new(create_faux_core(
                RegisterFauxProviderOptions::default(),
            ))),
        });
        let models = create_models(Some(CreateModelsOptions {
            models_store: store,
            ..Default::default()
        }));
        models.set_provider(provider);
        let controller = crate::utils::abort::AbortController::new();
        let pending = tokio::spawn(models.refresh(ModelsRefreshOptions {
            providers: Some(vec!["dynamic".to_owned()]),
            signal: Some(controller.signal()),
            ..Default::default()
        }));
        auth_started.notified().await;
        assert!(models.get_model("dynamic", "cached").is_some());
        controller.abort(AbortReason::default_abort());
        let result = pending
            .await
            .expect("refresh task")
            .expect("aborted refresh result");
        assert!(result.aborted);
        assert!(result.errors.is_empty());

        let check_started = Arc::new(Notify::new());
        let check_notice = check_started.clone();
        let blocked_check = ApiKeyAuth {
            name: "Blocked check".to_owned(),
            login: None,
            check: Some(Arc::new(move |_| {
                let notice = check_notice.clone();
                Box::pin(async move {
                    notice.notify_one();
                    futures::future::pending::<Result<Option<AuthCheck>, AuthError>>().await
                })
            })),
            resolve: Arc::new(|_| Box::pin(async { Ok(None) })),
        };
        let available_models = create_models(None);
        available_models.set_provider(runtime_provider(
            "blocked",
            ProviderAuth {
                api_key: Some(blocked_check),
                oauth: None,
            },
            Arc::new(|| Ok(vec![test_model("blocked", "m")])),
            None,
        ));
        let controller = crate::utils::abort::AbortController::new();
        let available = tokio::spawn(available_models.get_available(
            None,
            AuthOperationOptions {
                signal: Some(controller.signal()),
            },
        ));
        check_started.notified().await;
        controller.abort(AbortReason::default_abort());
        assert!(matches!(
            available.await.expect("available task"),
            Err(ModelsOperationError::Abort(_))
        ));
    }

    /// Pins pi `src/models.ts:574-575`: provider login rejections propagate unchanged.
    #[tokio::test]
    async fn login_propagates_api_key_and_oauth_errors_raw() {
        let api_error = AuthError {
            name: "ApiLoginError".to_owned(),
            message: "api login rejected".to_owned(),
            code: Some("api_login_code".to_owned()),
        };
        let api_callback_error = api_error.clone();
        let api_auth = ApiKeyAuth {
            name: "API login".to_owned(),
            login: Some(Arc::new(move |_| {
                let error = api_callback_error.clone();
                Box::pin(async move { Err(error) })
            })),
            check: None,
            resolve: Arc::new(|_| Box::pin(async { Ok(None) })),
        };

        let oauth_error = AuthError {
            name: "OAuthLoginError".to_owned(),
            message: "oauth login rejected".to_owned(),
            code: Some("oauth_login_code".to_owned()),
        };
        let oauth_callback_error = oauth_error.clone();
        let oauth_auth = OAuthAuth {
            name: "OAuth login".to_owned(),
            is_subscription: None,
            login_label: None,
            login: Arc::new(move |_| {
                let error = oauth_callback_error.clone();
                Box::pin(async move { Err(error) })
            }),
            refresh: Arc::new(|credential, _| Box::pin(async move { Ok(credential) })),
            to_auth: Arc::new(|credential| {
                Box::pin(async move {
                    Ok(ModelAuth {
                        api_key: Some(credential.access),
                        ..Default::default()
                    })
                })
            }),
        };

        let models = create_models(None);
        models.set_provider(runtime_provider(
            "api-login",
            ProviderAuth {
                api_key: Some(api_auth),
                oauth: None,
            },
            Arc::new(|| Ok(Vec::new())),
            None,
        ));
        models.set_provider(runtime_provider(
            "oauth-login",
            ProviderAuth {
                api_key: None,
                oauth: Some(oauth_auth),
            },
            Arc::new(|| Ok(Vec::new())),
            None,
        ));
        let interaction = Arc::new(FixedInteraction {
            signal: None,
            answer: "unused".to_owned(),
        });

        assert_eq!(
            models
                .login("api-login", AuthType::ApiKey, interaction.clone())
                .await,
            Err(ModelsOperationError::Auth(api_error))
        );
        assert_eq!(
            models
                .login("oauth-login", AuthType::OAuth, interaction)
                .await,
            Err(ModelsOperationError::Auth(oauth_error))
        );
    }

    /// Pins pi `src/models.ts:576-609`: abort stops rejecting once mutation starts.
    #[tokio::test]
    async fn login_commits_and_returns_after_abort_during_started_mutation() {
        let credentials = Arc::new(PointOfNoReturnCredentialStore::default());
        let expected = Credential::ApiKey(ApiKeyCredential {
            kind: ApiKeyCredentialType::ApiKey,
            key: Some("persisted".to_owned()),
            env: None,
        });
        let login_credential = expected.clone();
        let api_auth = ApiKeyAuth {
            name: "API login".to_owned(),
            login: Some(Arc::new(move |_| {
                let credential = login_credential.clone();
                Box::pin(async move {
                    let Credential::ApiKey(credential) = credential else {
                        unreachable!("test credential is api-key")
                    };
                    Ok(credential)
                })
            })),
            check: None,
            resolve: Arc::new(|_| Box::pin(async { Ok(None) })),
        };
        let models = create_models(Some(CreateModelsOptions {
            credentials: credentials.clone(),
            ..Default::default()
        }));
        models.set_provider(runtime_provider(
            "p1",
            ProviderAuth {
                api_key: Some(api_auth),
                oauth: None,
            },
            Arc::new(|| Ok(Vec::new())),
            None,
        ));
        let controller = crate::utils::abort::AbortController::new();
        let interaction = Arc::new(FixedInteraction {
            signal: Some(controller.signal()),
            answer: "unused".to_owned(),
        });
        let login = tokio::spawn(models.login("p1", AuthType::ApiKey, interaction));

        credentials.callback_returned.notified().await;
        controller.abort(AbortReason::new("AbortError", "late abort"));
        tokio::task::yield_now().await;
        assert!(!login.is_finished());
        credentials.release_mutation.notify_one();

        assert_eq!(
            login.await.expect("login task").expect("login succeeds"),
            expected
        );
        assert_eq!(
            credentials
                .read("p1".to_owned(), AuthOperationOptions::default())
                .await
                .expect("stored credential"),
            Some(expected)
        );
    }

    /// Pins pi `src/models.ts:458-472`: refresh auth failures remain raw errors.
    #[tokio::test]
    async fn refresh_preserves_raw_oauth_refresh_api_resolve_and_store_modify_errors() {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        credentials
            .modify(
                "oauth-refresh".to_owned(),
                Box::new(|_| Box::pin(async { Ok(Some(oauth_credential("old", 0.0))) })),
                AuthOperationOptions::default(),
            )
            .await
            .expect("seed oauth");

        let oauth_error = AuthError {
            name: "OAuthRefreshError".to_owned(),
            message: "refresh rejected".to_owned(),
            code: Some("refresh_code".to_owned()),
        };
        let oauth_callback_error = oauth_error.clone();
        let oauth = oauth_auth(
            Arc::new(move |_, _| {
                let error = oauth_callback_error.clone();
                Box::pin(async move { Err(error) })
            }),
            Arc::new(|credential| {
                Box::pin(async move {
                    Ok(ModelAuth {
                        api_key: Some(credential.access),
                        ..Default::default()
                    })
                })
            }),
        );

        let api_error = AuthError {
            name: "ApiResolveError".to_owned(),
            message: "resolve rejected".to_owned(),
            code: Some("resolve_code".to_owned()),
        };
        let api_callback_error = api_error.clone();
        let api_auth = ApiKeyAuth {
            name: "API resolve".to_owned(),
            login: None,
            check: None,
            resolve: Arc::new(move |_| {
                let error = api_callback_error.clone();
                Box::pin(async move { Err(error) })
            }),
        };

        let refresh: RuntimeRefresh = Arc::new(|_| Box::pin(async { Ok(()) }));
        let models = create_models(Some(CreateModelsOptions {
            credentials,
            ..Default::default()
        }));
        models.set_provider(runtime_provider(
            "oauth-refresh",
            ProviderAuth {
                api_key: None,
                oauth: Some(oauth),
            },
            Arc::new(|| Ok(Vec::new())),
            Some(refresh.clone()),
        ));
        models.set_provider(runtime_provider(
            "api-resolve",
            ProviderAuth {
                api_key: Some(api_auth),
                oauth: None,
            },
            Arc::new(|| Ok(Vec::new())),
            Some(refresh),
        ));

        let result = models
            .refresh(ModelsRefreshOptions::default())
            .await
            .expect("refresh returns errors in-band");
        assert_eq!(
            result.errors["oauth-refresh"],
            ModelsOperationError::Auth(oauth_error)
        );
        assert_eq!(
            result.errors["api-resolve"],
            ModelsOperationError::Auth(api_error)
        );

        let store_error = AuthError {
            name: "CredentialStoreError".to_owned(),
            message: "store modify rejected".to_owned(),
            code: Some("store_code".to_owned()),
        };
        let rejecting_store = Arc::new(RejectingModifyCredentialStore {
            credential: oauth_credential("old", 0.0),
            error: store_error.clone(),
        });
        let store_models = create_models(Some(CreateModelsOptions {
            credentials: rejecting_store,
            ..Default::default()
        }));
        store_models.set_provider(runtime_provider(
            "oauth-store",
            ProviderAuth {
                api_key: None,
                oauth: Some(oauth_auth(
                    Arc::new(|credential, _| Box::pin(async move { Ok(credential) })),
                    Arc::new(|credential| {
                        Box::pin(async move {
                            Ok(ModelAuth {
                                api_key: Some(credential.access),
                                ..Default::default()
                            })
                        })
                    }),
                )),
            },
            Arc::new(|| Ok(Vec::new())),
            Some(Arc::new(|_| Box::pin(async { Ok(()) }))),
        ));
        let result = store_models
            .refresh(ModelsRefreshOptions::default())
            .await
            .expect("refresh returns store error in-band");
        assert_eq!(
            result.errors["oauth-store"],
            ModelsOperationError::Auth(store_error)
        );
    }

    /// Ports pi `test/models-runtime.test.ts:617-647,777-963`.
    #[tokio::test]
    async fn auth_check_login_logout_availability_and_oauth_refresh_match_runtime_contract() {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let callback_signals = Arc::new(AtomicUsize::new(0));
        let resolve_calls = callback_signals.clone();
        let check_calls = callback_signals.clone();
        let login_calls = callback_signals.clone();
        let api_key = ApiKeyAuth {
            name: "Test API key".to_owned(),
            login: Some(Arc::new(move |interaction| {
                let login_calls = login_calls.clone();
                Box::pin(async move {
                    assert!(!interaction.signal.is_aborted());
                    login_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(ApiKeyCredential {
                        kind: ApiKeyCredentialType::ApiKey,
                        key: Some("logged-in".to_owned()),
                        env: None,
                    })
                })
            })),
            check: Some(Arc::new(move |input| {
                let check_calls = check_calls.clone();
                Box::pin(async move {
                    assert!(!input.signal.is_aborted());
                    check_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(input.credential.map(|_| AuthCheck {
                        source: Some("stored".to_owned()),
                        kind: AuthType::ApiKey,
                    }))
                })
            })),
            resolve: Arc::new(move |input| {
                let resolve_calls = resolve_calls.clone();
                Box::pin(async move {
                    assert!(!input.signal.is_aborted());
                    resolve_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(input.credential.and_then(|credential| {
                        credential.key.map(|key| AuthResult {
                            auth: ModelAuth {
                                api_key: Some(key),
                                ..Default::default()
                            },
                            env: credential.env,
                            source: Some("stored".to_owned()),
                        })
                    }))
                })
            }),
        };
        let models = create_models(Some(CreateModelsOptions {
            credentials: credentials.clone(),
            ..Default::default()
        }));
        models.set_provider(runtime_provider(
            "p1",
            ProviderAuth {
                api_key: Some(api_key),
                oauth: None,
            },
            Arc::new(|| Ok(vec![test_model("p1", "model-a")])),
            None,
        ));
        assert!(
            models
                .get_available(None, AuthOperationOptions::default())
                .await
                .expect("available")
                .is_empty()
        );
        let controller = crate::utils::abort::AbortController::new();
        let interaction = Arc::new(FixedInteraction {
            signal: Some(controller.signal()),
            answer: "unused".to_owned(),
        });
        let credential = models
            .login("p1", AuthType::ApiKey, interaction)
            .await
            .expect("login");
        assert_eq!(
            credential,
            Credential::ApiKey(ApiKeyCredential {
                kind: ApiKeyCredentialType::ApiKey,
                key: Some("logged-in".to_owned()),
                env: None,
            })
        );
        assert_eq!(
            models
                .check_auth(
                    "p1",
                    AuthOperationOptions {
                        signal: Some(controller.signal()),
                    },
                )
                .await
                .expect("check")
                .expect("configured")
                .kind,
            AuthType::ApiKey
        );
        assert_eq!(
            models
                .get_auth(
                    "p1",
                    AuthResolutionOverrides {
                        signal: Some(controller.signal()),
                        ..Default::default()
                    },
                )
                .await
                .expect("auth")
                .expect("configured")
                .auth
                .api_key
                .as_deref(),
            Some("logged-in")
        );
        assert_eq!(
            models
                .get_available(Some("p1".to_owned()), AuthOperationOptions::default())
                .await
                .expect("available")
                .len(),
            1
        );
        models
            .logout("p1", AuthOperationOptions::default())
            .await
            .expect("logout");
        assert!(
            credentials
                .read("p1".to_owned(), AuthOperationOptions::default())
                .await
                .expect("credential read")
                .is_none()
        );
        assert!(callback_signals.load(Ordering::SeqCst) >= 4);

        let oauth_credentials = Arc::new(InMemoryCredentialStore::default());
        oauth_credentials
            .modify(
                "oauth".to_owned(),
                Box::new(|_| Box::pin(async { Ok(Some(oauth_credential("old", 0.0))) })),
                AuthOperationOptions::default(),
            )
            .await
            .expect("seed oauth");
        let refreshes = Arc::new(AtomicUsize::new(0));
        let refresh_calls = refreshes.clone();
        let oauth = oauth_auth(
            Arc::new(move |mut credential, signal| {
                let refresh_calls = refresh_calls.clone();
                Box::pin(async move {
                    assert!(!signal.is_aborted());
                    refresh_calls.fetch_add(1, Ordering::SeqCst);
                    credential.access = "fresh".to_owned();
                    credential.expires = now_ms() + 3_600_000.0;
                    Ok(credential)
                })
            }),
            Arc::new(|credential| {
                Box::pin(async move {
                    Ok(ModelAuth {
                        api_key: Some(credential.access),
                        ..Default::default()
                    })
                })
            }),
        );
        let oauth_models = create_models(Some(CreateModelsOptions {
            credentials: oauth_credentials.clone(),
            ..Default::default()
        }));
        oauth_models.set_provider(runtime_provider(
            "oauth",
            ProviderAuth {
                api_key: None,
                oauth: Some(oauth),
            },
            Arc::new(|| Ok(vec![test_model("oauth", "m")])),
            None,
        ));
        assert_eq!(
            oauth_models
                .check_auth("oauth", AuthOperationOptions::default())
                .await
                .expect("check")
                .expect("oauth configured")
                .kind,
            AuthType::OAuth
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 0);
        assert_eq!(
            oauth_models
                .get_auth("oauth", AuthResolutionOverrides::default())
                .await
                .expect("oauth auth")
                .expect("configured")
                .auth
                .api_key
                .as_deref(),
            Some("fresh")
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert!(matches!(
            oauth_credentials
                .read("oauth".to_owned(), AuthOperationOptions::default())
                .await
                .expect("stored"),
            Some(Credential::OAuth(OAuthCredential { access, .. })) if access == "fresh"
        ));
    }

    /// Ports pi `test/providers.test.ts:344-424,501-509`.
    #[tokio::test]
    async fn created_provider_dispatches_by_api_and_errors_for_missing_implementations() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let api_a = CaptureStreams {
            calls: calls.clone(),
        };
        let api_b = CaptureStreams {
            calls: calls.clone(),
        };
        let provider = create_provider(CreateProviderOptions {
            id: "mixed".to_owned(),
            name: None,
            base_url: None,
            headers: None,
            auth: ambient_auth(),
            models: vec![test_model("mixed", "a"), test_model("mixed", "b")],
            fetch_models: None,
            filter_models: None,
            api: ProviderApi::ByApi(IndexMap::from([
                (
                    Api::from("api-a"),
                    Arc::new(api_a) as Arc<dyn ProviderStreams>,
                ),
                (
                    Api::from("api-b"),
                    Arc::new(api_b) as Arc<dyn ProviderStreams>,
                ),
            ])),
        });
        let mut a = test_model("mixed", "a");
        a.api = "api-a".into();
        let mut b = test_model("mixed", "b");
        b.api = "api-b".into();
        assert_eq!(
            provider
                .stream_simple(&a, &Context::default(), SimpleStreamOptions::default())
                .result()
                .await
                .expect("a")
                .stop_reason,
            StopReason::Stop
        );
        assert_eq!(
            provider
                .stream_simple(&b, &Context::default(), SimpleStreamOptions::default())
                .result()
                .await
                .expect("b")
                .stop_reason,
            StopReason::Stop
        );
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .map(|(model, _)| model.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        let mut missing = test_model("mixed", "ghost");
        missing.api = "api-ghost".into();
        let error = provider
            .stream_simple(
                &missing,
                &Context::default(),
                SimpleStreamOptions::default(),
            )
            .result()
            .await
            .expect("terminal error");
        assert_eq!(error.stop_reason, StopReason::Error);
        assert!(
            error
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("no API implementation"))
        );
        assert!(!provider.supports_fetch_deferred());
        assert!(!provider.supports_cancel_deferred());
    }

    /// Pins pi `src/models.ts:902-943` helper behavior not covered by one runtime case.
    #[test]
    fn thinking_clamp_and_model_equality_match_pi() {
        let mut model = test_model("p", "m");
        assert_eq!(
            get_supported_thinking_levels(&model),
            [ModelThinkingLevel::Off]
        );
        model.reasoning = true;
        model.thinking_level_map = Some(ThinkingLevelMap {
            off: Some(None),
            minimal: Some(None),
            low: Some(Some("low".to_owned())),
            medium: Some(None),
            high: Some(Some("high".to_owned())),
            xhigh: Some(None),
            max: Some(Some("max".to_owned())),
        });
        assert_eq!(
            get_supported_thinking_levels(&model),
            [
                ModelThinkingLevel::Low,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Max
            ]
        );
        assert_eq!(
            clamp_thinking_level(&model, ModelThinkingLevel::Medium),
            ModelThinkingLevel::High
        );
        assert!(models_are_equal(Some(&model), Some(&model.clone())));
        let mut other = model.clone();
        other.provider = ProviderId::from("other");
        assert!(!models_are_equal(Some(&model), Some(&other)));
        assert!(!models_are_equal(Some(&model), None));
    }
}
