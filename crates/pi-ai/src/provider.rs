//! Provider registration, API dispatch, and HTTP request establishment from
//! Architecture v2 part 1 §3.5–§3.6 as revised by part 2 §2.4–§2.6 and §3.2.

#![allow(
    clippy::result_large_err,
    reason = "AiError retains the architecture-specified provider and ModelRef fields"
)]

use crate::{
    ApiId, AssistantStream, AttemptFailure, AttemptMiddleware, AuthError, CancellationToken,
    Context, DefaultRetryClassifier, ErasedApiOptionsPatch, ErasedPayloadContext,
    ErasedPayloadTransform, HeaderMapSpec, HttpRequest, HttpResponse, HttpTransport,
    LocalAssistantStream, LocalAttemptMiddleware, LocalBoxFuture, LocalDefaultRetryClassifier,
    LocalDefaultRetrySleeper, LocalErasedPayloadTransform, LocalHttpResponse, LocalHttpTransport,
    LocalManagedModelCatalog, LocalModelCatalogSource, LocalProviderCatalogState,
    LocalResponseObserver, LocalRetryClassifier, LocalRetrySleeper, ManagedModelCatalog,
    ModelCatalogSource, ModelDescriptor, ModelRef, PayloadTransformDisposition,
    ProviderCatalogState, ProviderId, ProviderPayload, ProviderResponseMetadata, RequestStartError,
    RequestStartErrorKind, ResponseObservationContext, ResponseObserver, RetryClassifier,
    RetryDecision, RetryPolicy, RetrySleeper, SecretString, SendBoxFuture, SimpleGenerationOptions,
    apply_header_spec, establish_with_retry_and_local_sleeper, establish_with_retry_and_sleeper,
    request_id_from_headers,
};
use futures_util::future::{Either, select};
use http::HeaderMap;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use url::Url;

/// Public provider identity and logical request defaults.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderDescriptor {
    /// Open provider identifier.
    pub id: ProviderId,
    /// Human-readable provider name.
    pub display_name: String,
    /// Optional provider-wide endpoint override.
    pub base_url: Option<Url>,
    /// Provider/API default logical headers.
    pub headers: HeaderMapSpec,
}

impl fmt::Debug for ProviderDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderDescriptor")
            .field("id", &self.id)
            .field("display_name", &self.display_name)
            .field(
                "base_url",
                &self.base_url.as_ref().map(|_| "<redacted endpoint>"),
            )
            .field("headers", &"<redacted headers>")
            .finish()
    }
}

impl ProviderDescriptor {
    /// Creates a descriptor whose display name is its identifier.
    pub fn new(id: impl Into<ProviderId>) -> Self {
        let id = id.into();
        Self {
            display_name: id.as_str().to_owned(),
            id,
            base_url: None,
            headers: HeaderMapSpec::new(),
        }
    }
}

/// Open label describing where effective authentication came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthSource(pub String);

impl AuthSource {
    /// Creates an authentication-source label.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Authentication resolution result consumed by the request pipeline.
#[derive(Clone)]
pub struct ResolvedAuth {
    /// Provider API secret when the API implementation cannot use a header.
    pub api_key: Option<SecretString>,
    /// Provider/auth logical headers.
    pub headers: HeaderMap,
    /// Credential-specific endpoint override.
    pub base_url: Option<Url>,
    /// Source label for status and diagnostics.
    pub source: AuthSource,
}

impl fmt::Debug for ResolvedAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedAuth")
            .field("api_key", &self.api_key)
            .field("headers", &"<redacted logical headers>")
            .field(
                "base_url",
                &self.base_url.as_ref().map(|_| "<redacted endpoint>"),
            )
            .field("source", &self.source)
            .finish()
    }
}

/// Control-plane operation requesting provider authentication.
///
/// Pinned Pi deliberately uses different OAuth refresh policies for ordinary
/// requests and catalog refreshes: request auth refreshes within its minimum
/// validity window and applies a refresh timeout, while catalog auth refreshes
/// only an actually expired token and has no request-auth timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthResolutionPurpose {
    /// An ordinary model request or explicit request-auth lookup.
    Request,
    /// Credential resolution for a dynamic catalog network refresh.
    CatalogRefresh,
}

/// Owned context passed to provider authentication resolution.
#[derive(Clone)]
pub struct ResolveAuthRequest {
    /// Registered provider metadata.
    pub provider: ProviderDescriptor,
    /// Current catalog model for request execution. Provider-scoped lookups
    /// and catalog refreshes use `None`; [`Self::purpose`] distinguishes them.
    pub model: Option<ModelDescriptor>,
    /// Operation-specific OAuth refresh policy.
    pub purpose: AuthResolutionPurpose,
    /// Models-owned credential transaction capability.
    pub credential_store: Arc<dyn crate::CredentialStore>,
    /// Host-owned ambient environment/filesystem capability.
    pub auth_context: Arc<dyn crate::AuthContext>,
    /// Explicit per-request values and OAuth validity requirement.
    pub overrides: crate::AuthResolutionOverrides,
}

impl ResolveAuthRequest {
    /// Creates a request with an empty in-memory store and ambient context.
    /// Models replaces both capabilities with its configured instances.
    pub fn isolated(provider: ProviderDescriptor, model: Option<ModelDescriptor>) -> Self {
        Self {
            provider,
            model,
            purpose: AuthResolutionPurpose::Request,
            credential_store: Arc::new(crate::InMemoryCredentialStore::default()),
            auth_context: Arc::new(crate::EmptyAuthContext),
            overrides: crate::AuthResolutionOverrides::default(),
        }
    }
}

impl fmt::Debug for ResolveAuthRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolveAuthRequest")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("purpose", &self.purpose)
            .field("credential_store", &"<credential store>")
            .field("auth_context", &"<auth context>")
            .field("overrides", &self.overrides)
            .finish()
    }
}

/// Local-executor context passed to provider authentication resolution.
#[derive(Clone)]
pub struct LocalResolveAuthRequest {
    /// Registered provider metadata.
    pub provider: ProviderDescriptor,
    /// Current catalog model for request execution. Provider-scoped lookups
    /// and catalog refreshes use `None`; [`Self::purpose`] distinguishes them.
    pub model: Option<ModelDescriptor>,
    /// Operation-specific OAuth refresh policy.
    pub purpose: AuthResolutionPurpose,
    /// Models-owned local credential transaction capability.
    pub credential_store: Rc<dyn crate::LocalCredentialStore>,
    /// Host-owned local ambient environment/filesystem capability.
    pub auth_context: Rc<dyn crate::LocalAuthContext>,
    /// Explicit per-request values and OAuth validity requirement.
    pub overrides: crate::AuthResolutionOverrides,
}

impl LocalResolveAuthRequest {
    /// Creates an isolated local request with empty capabilities.
    pub fn isolated(provider: ProviderDescriptor, model: Option<ModelDescriptor>) -> Self {
        Self {
            provider,
            model,
            purpose: AuthResolutionPurpose::Request,
            credential_store: Rc::new(crate::LocalInMemoryCredentialStore::default()),
            auth_context: Rc::new(crate::EmptyAuthContext),
            overrides: crate::AuthResolutionOverrides::default(),
        }
    }
}

impl fmt::Debug for LocalResolveAuthRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalResolveAuthRequest")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("purpose", &self.purpose)
            .field("credential_store", &"<local credential store>")
            .field("auth_context", &"<local auth context>")
            .field("overrides", &self.overrides)
            .finish()
    }
}

/// Provider-owned authentication resolution, login, and optional cleanup.
/// Credential persistence remains a Models control-plane responsibility.
pub trait AuthResolver: Send + Sync + 'static {
    /// Resolves current credentials and provider-owned request defaults.
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>>;

    /// Runs provider-owned interactive login. Models persists the returned
    /// credential under a store lease.
    fn login(
        &self,
        interaction: Arc<dyn crate::AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<crate::Credential, AuthError>> {
        let _ = interaction;
        let _ = cancellation;
        Box::pin(async {
            Err(AuthError::UnsupportedLogin {
                message: "provider does not support interactive login".into(),
            })
        })
    }

    /// Performs provider-owned logout cleanup before Models deletes the
    /// credential. Most providers use the no-op default.
    fn logout(&self, _cancellation: CancellationToken) -> SendBoxFuture<'_, Result<(), AuthError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Single-threaded provider-owned request authentication resolution.
pub trait LocalAuthResolver: 'static {
    /// Resolves current credentials and provider-owned request defaults.
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>>;

    /// Runs local provider-owned interactive login.
    fn login(
        &self,
        interaction: Rc<dyn crate::LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<crate::Credential, AuthError>> {
        let _ = interaction;
        let _ = cancellation;
        Box::pin(async {
            Err(AuthError::UnsupportedLogin {
                message: "provider does not support interactive login".into(),
            })
        })
    }

    /// Performs local provider-owned logout cleanup.
    fn logout(
        &self,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), AuthError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Keyless provider authentication that always resolves successfully.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnonymousAuthResolver;

impl AuthResolver for AnonymousAuthResolver {
    fn resolve(
        &self,
        _request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async {
            Ok(Some(ResolvedAuth {
                api_key: None,
                headers: HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("ambient"),
            }))
        })
    }
}

impl LocalAuthResolver for AnonymousAuthResolver {
    fn resolve(
        &self,
        _request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async {
            Ok(Some(ResolvedAuth {
                api_key: None,
                headers: HeaderMap::new(),
                base_url: None,
                source: AuthSource::new("ambient"),
            }))
        })
    }
}

/// Synchronous immutable model snapshot owned by one provider registration.
pub trait ModelCatalog: Send + Sync + 'static {
    /// Returns the latest complete published snapshot.
    fn snapshot(&self) -> Arc<[ModelDescriptor]>;

    /// Returns managed provenance state when this catalog participates in the
    /// Models catalog control plane.
    fn catalog_state(&self) -> Option<Arc<ProviderCatalogState>> {
        None
    }

    /// Returns the dynamic source, absent for static catalogs.
    fn catalog_source(&self) -> Option<Arc<dyn ModelCatalogSource>> {
        None
    }
}

/// Single-threaded synchronous immutable model snapshot.
pub trait LocalModelCatalog: 'static {
    /// Returns the latest complete published snapshot.
    fn snapshot(&self) -> Rc<[ModelDescriptor]>;

    /// Returns local managed provenance state when this catalog participates
    /// in the LocalModels catalog control plane.
    fn catalog_state(&self) -> Option<Rc<LocalProviderCatalogState>> {
        None
    }

    /// Returns the non-`Send` dynamic source, absent for static catalogs.
    fn catalog_source(&self) -> Option<Rc<dyn LocalModelCatalogSource>> {
        None
    }
}

/// Immutable catalog used by static providers and hermetic tests.
#[derive(Clone)]
pub struct StaticModelCatalog {
    models: Arc<[ModelDescriptor]>,
}

impl fmt::Debug for StaticModelCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StaticModelCatalog")
            .field("model_count", &self.models.len())
            .finish()
    }
}

impl StaticModelCatalog {
    /// Creates a static model catalog.
    pub fn new(models: impl Into<Vec<ModelDescriptor>>) -> Self {
        Self {
            models: Arc::from(models.into()),
        }
    }
}

impl ModelCatalog for StaticModelCatalog {
    fn snapshot(&self) -> Arc<[ModelDescriptor]> {
        Arc::clone(&self.models)
    }
}

/// Immutable local catalog used by local providers and `Rc`-based hosts.
#[derive(Clone)]
pub struct LocalStaticModelCatalog {
    models: Rc<[ModelDescriptor]>,
}

impl fmt::Debug for LocalStaticModelCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalStaticModelCatalog")
            .field("model_count", &self.models.len())
            .finish()
    }
}

impl LocalStaticModelCatalog {
    /// Creates a static local model catalog.
    pub fn new(models: impl Into<Vec<ModelDescriptor>>) -> Self {
        Self {
            models: Rc::from(models.into()),
        }
    }
}

impl LocalModelCatalog for LocalStaticModelCatalog {
    fn snapshot(&self) -> Rc<[ModelDescriptor]> {
        Rc::clone(&self.models)
    }
}

/// Provider-neutral internal error used by API and provider composition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AiError {
    /// Stable error classification.
    pub kind: AiErrorKind,
    /// Sanitized diagnostic text.
    pub message: String,
    /// Provider involved, when known.
    pub provider: Option<ProviderId>,
    /// Model involved, when known.
    pub model: Option<ModelRef>,
    /// Whether a higher-level logical operation may retry.
    pub retryable: bool,
    /// Provider-requested delay, when retained.
    pub retry_after: Option<std::time::Duration>,
    /// Provider-native error code when safe to expose.
    pub provider_code: Option<String>,
    /// HTTP status retained from the original failed attempt.
    pub status: Option<u16>,
    /// Provider request identifier retained from response headers.
    pub request_id: Option<String>,
    /// Zero-based transport attempt that produced the final failure.
    pub attempt: Option<u32>,
}

impl AiError {
    /// Creates an internal provider/API error.
    pub fn new(kind: AiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider: None,
            model: None,
            retryable: false,
            retry_after: None,
            provider_code: None,
            status: None,
            request_id: None,
            attempt: None,
        }
    }

    /// Adds current model identity.
    pub fn with_model(mut self, model: ModelRef) -> Self {
        self.provider = Some(model.provider.clone());
        self.model = Some(model);
        self
    }

    pub(crate) fn into_request_start(self) -> RequestStartError {
        let kind = match self.kind {
            AiErrorKind::InvalidRequest | AiErrorKind::Protocol => {
                RequestStartErrorKind::InvalidRequest
            }
            AiErrorKind::UnknownProvider => RequestStartErrorKind::UnknownProvider,
            AiErrorKind::UnknownModel => RequestStartErrorKind::UnknownModel,
            AiErrorKind::Cancelled => RequestStartErrorKind::Cancelled,
            AiErrorKind::Authentication
            | AiErrorKind::Authorization
            | AiErrorKind::RateLimited
            | AiErrorKind::Transport
            | AiErrorKind::ProviderRejected => RequestStartErrorKind::RuntimeUnavailable,
            AiErrorKind::Internal => RequestStartErrorKind::Internal,
        };
        RequestStartError {
            kind,
            message: self.message,
            provider: self.provider,
            model: self.model,
        }
    }
}

impl fmt::Display for AiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AiError {}

/// Stable provider/API error categories.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AiErrorKind {
    /// Invalid canonical request.
    InvalidRequest,
    /// Provider is not registered.
    UnknownProvider,
    /// Model is not registered.
    UnknownModel,
    /// Authentication failed or is absent.
    Authentication,
    /// Provider authorization rejected the request.
    Authorization,
    /// Provider rate limited the request.
    RateLimited,
    /// HTTP or SDK transport failure.
    Transport,
    /// Provider rejected an otherwise valid request.
    ProviderRejected,
    /// Provider protocol violation.
    Protocol,
    /// Caller cancelled the request.
    Cancelled,
    /// Internal invariant or middleware failure.
    Internal,
}

/// Resolved request passed from [`crate::Models`] to one API implementation.
pub struct ResolvedApiRequest {
    /// Current model descriptor.
    pub model: ModelDescriptor,
    /// Canonical context. API-family handlers own projection and lowering.
    pub context: Context,
    /// Provider-neutral options and at most one erased API patch.
    pub options: SimpleGenerationOptions,
    /// Effective endpoint after auth resolution.
    pub endpoint: Url,
    /// Final logical headers after all header transforms.
    pub headers: HeaderMap,
    /// Resolved provider secret, when required outside headers.
    pub api_key: Option<SecretString>,
    /// Selected API identifier.
    pub api: ApiId,
    /// Logical payload transforms in registration order.
    pub payload_transforms: Arc<[Arc<dyn ErasedPayloadTransform>]>,
    /// Attempt-independent response observers in registration order.
    pub response_observers: Arc<[Arc<dyn ResponseObserver>]>,
    /// Attempt middleware in registration order.
    pub attempt_middleware: Arc<[Arc<dyn AttemptMiddleware>]>,
    /// Resolved retry policy.
    pub retry_policy: RetryPolicy,
    /// Per-attempt HTTP response-establishment timeout.
    pub timeout: Option<Duration>,
    /// Provider-selected retry classifier.
    pub retry_classifier: Arc<dyn RetryClassifier>,
}

/// API execution resources available during lowering, transport, and decoding.
pub struct ApiExecutionContext<'a> {
    /// Current catalog model.
    pub model: &'a ModelDescriptor,
    /// Effective endpoint.
    pub endpoint: &'a Url,
    /// Final logical headers.
    pub headers: &'a HeaderMap,
    /// Resolved retry policy.
    pub retry_policy: &'a RetryPolicy,
    /// Provider-selected retry classifier.
    pub retry_classifier: &'a dyn RetryClassifier,
    /// Injected HTTP transport.
    pub transport: &'a dyn HttpTransport,
    /// Request cancellation token.
    pub cancellation: &'a CancellationToken,
    /// Resolved API key for SDK-style handlers that cannot consume a header.
    pub api_key: Option<&'a SecretString>,
}

/// Established provider response passed to the API-family stream decoder.
pub type ProviderResponseStream = HttpResponse;

/// Erased API-family lowering, encoding, and decoding contract.
pub trait ErasedApiHandler: Send + Sync + 'static {
    /// API-family identifier.
    fn api_id(&self) -> &ApiId;

    /// Projects context, lowers simple options, and encodes one logical payload.
    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError>;

    /// Converts an established response body into normalized assistant events.
    fn decode_stream(
        &self,
        response: ProviderResponseStream,
        execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream;
}

/// Provider/API dispatch unit. Concrete HTTP APIs can use [`HttpChatApi`]; SDK
/// and non-HTTP APIs implement this trait directly.
pub trait ChatApi: Send + Sync + 'static {
    /// Establishes a normalized assistant stream.
    fn stream(
        &self,
        request: ResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, AiError>>;
}

/// Standard HTTP API composition: erased handler + injected transport +
/// retry-establishment loop.
pub struct HttpChatApi {
    handler: Arc<dyn ErasedApiHandler>,
    transport: Arc<dyn HttpTransport>,
    sleeper: Arc<dyn RetrySleeper>,
}

impl fmt::Debug for HttpChatApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpChatApi")
            .finish_non_exhaustive()
    }
}

impl HttpChatApi {
    /// Creates an HTTP API with the default portable retry timer.
    pub fn new(handler: Arc<dyn ErasedApiHandler>, transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            handler,
            transport,
            sleeper: Arc::new(crate::DefaultRetrySleeper),
        }
    }

    /// Replaces the retry sleeper, primarily for hermetic host clocks and tests.
    pub fn with_retry_sleeper(mut self, sleeper: Arc<dyn RetrySleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    async fn execute(
        &self,
        request: ResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> Result<AssistantStream, AiError> {
        cancellation.check().map_err(|_| {
            AiError::new(AiErrorKind::Cancelled, "request cancelled")
                .with_model(request.model.common.model_ref.clone())
        })?;

        let execution = ApiExecutionContext {
            model: &request.model,
            endpoint: &request.endpoint,
            headers: &request.headers,
            retry_policy: &request.retry_policy,
            retry_classifier: request.retry_classifier.as_ref(),
            transport: self.transport.as_ref(),
            cancellation: &cancellation,
            api_key: request.api_key.as_ref(),
        };
        let mut payload = self.handler.lower_and_encode(
            &request.model,
            &request.context,
            &request.options,
            request.options.api_options.as_ref(),
            &execution,
        )?;

        let payload_context = ErasedPayloadContext {
            model: &request.model.common.model_ref,
            api: &request.api,
            endpoint: &request.endpoint,
            headers: &request.headers,
        };
        for transform in request.payload_transforms.iter() {
            match transform
                .transform(payload_context, &mut payload)
                .await
                .map_err(|error| middleware_ai_error(error, &request.model.common.model_ref))?
            {
                PayloadTransformDisposition::Continue => {}
                PayloadTransformDisposition::Replace(replacement) => payload = replacement,
            }
        }

        let body = payload
            .encode_body()
            .map_err(|error| middleware_ai_error(error, &request.model.common.model_ref))?;

        let frozen = HttpRequest {
            method: payload.method,
            url: request.endpoint.clone(),
            headers: request.headers.clone(),
            body,
            timeout: request.timeout,
            attempt: 0,
        };

        let attempt_middleware = Arc::clone(&request.attempt_middleware);
        let response_observers = Arc::clone(&request.response_observers);
        let model_ref = request.model.common.model_ref.clone();
        let api_id = request.api.clone();
        let endpoint = request.endpoint.clone();
        let transport = Arc::clone(&self.transport);
        let response = establish_with_retry_and_sleeper(
            &request.retry_policy,
            request.retry_classifier.as_ref(),
            self.sleeper.as_ref(),
            &cancellation,
            |attempt| {
                let mut attempt_request = frozen.clone();
                let cancellation = cancellation.clone();
                let attempt_middleware = Arc::clone(&attempt_middleware);
                let response_observers = Arc::clone(&response_observers);
                let model_ref = model_ref.clone();
                let api_id = api_id.clone();
                let endpoint = endpoint.clone();
                let transport = Arc::clone(&transport);
                async move {
                    attempt_request.attempt = attempt;
                    for middleware in attempt_middleware.iter() {
                        middleware
                            .before_attempt(attempt, &mut attempt_request)
                            .await
                            .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                    }

                    let response = execute_transport_attempt(
                        transport.as_ref(),
                        attempt_request,
                        cancellation,
                    )
                    .await?;
                    let metadata = ProviderResponseMetadata {
                        attempt,
                        status: response.status,
                        headers: response.headers.clone(),
                        request_id: request_id_from_headers(&response.headers),
                    };
                    let observation = ResponseObservationContext {
                        model: &model_ref,
                        api: &api_id,
                        endpoint: &endpoint,
                    };
                    for observer in response_observers.iter() {
                        observer
                            .on_response(observation, &metadata)
                            .await
                            .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                    }
                    if !(200..300).contains(&response.status) {
                        return Err(AttemptFailure::http_at(
                            attempt,
                            response.status,
                            response.headers,
                            SystemTime::now(),
                            "provider rejected request before streaming",
                        ));
                    }
                    Ok(response)
                }
            },
        )
        .await
        .map_err(|error| {
            attempt_ai_error(
                error,
                &request.model.common.model_ref,
                request.retry_classifier.as_ref(),
                &request.retry_policy,
            )
        })?;

        let execution = ApiExecutionContext {
            model: &request.model,
            endpoint: &request.endpoint,
            headers: &request.headers,
            retry_policy: &request.retry_policy,
            retry_classifier: request.retry_classifier.as_ref(),
            transport: self.transport.as_ref(),
            cancellation: &cancellation,
            api_key: request.api_key.as_ref(),
        };
        Ok(self.handler.decode_stream(response, &execution))
    }
}

impl ChatApi for HttpChatApi {
    fn stream(
        &self,
        request: ResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, AiError>> {
        Box::pin(self.execute(request, cancellation))
    }
}

/// Resolved request passed from [`crate::LocalModels`] to a local API
/// implementation.
pub struct LocalResolvedApiRequest {
    /// Current model descriptor.
    pub model: ModelDescriptor,
    /// Canonical context.
    pub context: Context,
    /// Provider-neutral options and at most one erased API patch.
    pub options: SimpleGenerationOptions,
    /// Effective endpoint after auth resolution.
    pub endpoint: Url,
    /// Final logical headers.
    pub headers: HeaderMap,
    /// Resolved provider secret.
    pub api_key: Option<SecretString>,
    /// Selected API identifier.
    pub api: ApiId,
    /// Local logical payload transforms in registration order.
    pub payload_transforms: Rc<[Rc<dyn LocalErasedPayloadTransform>]>,
    /// Local response observers in registration order.
    pub response_observers: Rc<[Rc<dyn LocalResponseObserver>]>,
    /// Local attempt middleware in registration order.
    pub attempt_middleware: Rc<[Rc<dyn LocalAttemptMiddleware>]>,
    /// Resolved retry policy.
    pub retry_policy: RetryPolicy,
    /// Per-attempt HTTP response-establishment timeout.
    pub timeout: Option<Duration>,
    /// Provider-selected local retry classifier.
    pub retry_classifier: Rc<dyn LocalRetryClassifier>,
}

/// Local API execution resources available during lowering and decoding.
pub struct LocalApiExecutionContext<'a> {
    /// Current catalog model.
    pub model: &'a ModelDescriptor,
    /// Effective endpoint.
    pub endpoint: &'a Url,
    /// Final logical headers.
    pub headers: &'a HeaderMap,
    /// Resolved retry policy.
    pub retry_policy: &'a RetryPolicy,
    /// Provider-selected retry classifier.
    pub retry_classifier: &'a dyn LocalRetryClassifier,
    /// Injected local HTTP transport.
    pub transport: &'a dyn LocalHttpTransport,
    /// Request cancellation token.
    pub cancellation: &'a CancellationToken,
    /// Resolved API key for SDK-style handlers.
    pub api_key: Option<&'a SecretString>,
}

/// Established local provider response passed to the local decoder.
pub type LocalProviderResponseStream = LocalHttpResponse;

/// Single-threaded erased API-family lowering, encoding, and decoding contract.
pub trait LocalErasedApiHandler: 'static {
    /// API-family identifier.
    fn api_id(&self) -> &ApiId;

    /// Projects context, lowers options, and retains one typed logical payload.
    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError>;

    /// Converts an established local response into normalized assistant events.
    fn decode_stream(
        &self,
        response: LocalProviderResponseStream,
        execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream;
}

/// Single-threaded provider/API dispatch unit.
pub trait LocalChatApi: 'static {
    /// Establishes a normalized local assistant stream.
    fn stream(
        &self,
        request: LocalResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, AiError>>;
}

/// Standard local HTTP API composition.
pub struct LocalHttpChatApi {
    handler: Rc<dyn LocalErasedApiHandler>,
    transport: Rc<dyn LocalHttpTransport>,
    sleeper: Rc<dyn LocalRetrySleeper>,
}

impl fmt::Debug for LocalHttpChatApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalHttpChatApi")
            .finish_non_exhaustive()
    }
}

impl LocalHttpChatApi {
    /// Creates a local HTTP API with the default portable retry timer.
    pub fn new(
        handler: Rc<dyn LocalErasedApiHandler>,
        transport: Rc<dyn LocalHttpTransport>,
    ) -> Self {
        Self {
            handler,
            transport,
            sleeper: Rc::new(LocalDefaultRetrySleeper),
        }
    }

    /// Replaces the local retry sleeper.
    pub fn with_retry_sleeper(mut self, sleeper: Rc<dyn LocalRetrySleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    async fn execute(
        &self,
        request: LocalResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> Result<LocalAssistantStream, AiError> {
        cancellation.check().map_err(|_| {
            AiError::new(AiErrorKind::Cancelled, "request cancelled")
                .with_model(request.model.common.model_ref.clone())
        })?;

        let execution = LocalApiExecutionContext {
            model: &request.model,
            endpoint: &request.endpoint,
            headers: &request.headers,
            retry_policy: &request.retry_policy,
            retry_classifier: request.retry_classifier.as_ref(),
            transport: self.transport.as_ref(),
            cancellation: &cancellation,
            api_key: request.api_key.as_ref(),
        };
        let mut payload = self.handler.lower_and_encode(
            &request.model,
            &request.context,
            &request.options,
            request.options.api_options.as_ref(),
            &execution,
        )?;

        let payload_context = ErasedPayloadContext {
            model: &request.model.common.model_ref,
            api: &request.api,
            endpoint: &request.endpoint,
            headers: &request.headers,
        };
        for transform in request.payload_transforms.iter() {
            match transform
                .transform(payload_context, &mut payload)
                .await
                .map_err(|error| middleware_ai_error(error, &request.model.common.model_ref))?
            {
                PayloadTransformDisposition::Continue => {}
                PayloadTransformDisposition::Replace(replacement) => payload = replacement,
            }
        }

        let body = payload
            .encode_body()
            .map_err(|error| middleware_ai_error(error, &request.model.common.model_ref))?;
        let frozen = HttpRequest {
            method: payload.method,
            url: request.endpoint.clone(),
            headers: request.headers.clone(),
            body,
            timeout: request.timeout,
            attempt: 0,
        };

        let attempt_middleware = Rc::clone(&request.attempt_middleware);
        let response_observers = Rc::clone(&request.response_observers);
        let model_ref = request.model.common.model_ref.clone();
        let api_id = request.api.clone();
        let endpoint = request.endpoint.clone();
        let transport = Rc::clone(&self.transport);
        let response = establish_with_retry_and_local_sleeper(
            &request.retry_policy,
            request.retry_classifier.as_ref(),
            self.sleeper.as_ref(),
            &cancellation,
            |attempt| {
                let mut attempt_request = frozen.clone();
                let cancellation = cancellation.clone();
                let attempt_middleware = Rc::clone(&attempt_middleware);
                let response_observers = Rc::clone(&response_observers);
                let model_ref = model_ref.clone();
                let api_id = api_id.clone();
                let endpoint = endpoint.clone();
                let transport = Rc::clone(&transport);
                async move {
                    attempt_request.attempt = attempt;
                    for middleware in attempt_middleware.iter() {
                        middleware
                            .before_attempt(attempt, &mut attempt_request)
                            .await
                            .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                    }

                    let response = execute_local_transport_attempt(
                        transport.as_ref(),
                        attempt_request,
                        cancellation,
                    )
                    .await?;
                    let metadata = ProviderResponseMetadata {
                        attempt,
                        status: response.status,
                        headers: response.headers.clone(),
                        request_id: request_id_from_headers(&response.headers),
                    };
                    let observation = ResponseObservationContext {
                        model: &model_ref,
                        api: &api_id,
                        endpoint: &endpoint,
                    };
                    for observer in response_observers.iter() {
                        observer
                            .on_response(observation, &metadata)
                            .await
                            .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                    }
                    if !(200..300).contains(&response.status) {
                        return Err(AttemptFailure::http_at(
                            attempt,
                            response.status,
                            response.headers,
                            SystemTime::now(),
                            "provider rejected request before streaming",
                        ));
                    }
                    Ok(response)
                }
            },
        )
        .await
        .map_err(|error| {
            let original = error.original();
            let decision = match &error {
                AttemptFailure::RetryDelayTooLong {
                    requested, maximum, ..
                } => RetryDecision::RejectServerDelay {
                    requested: *requested,
                    maximum: *maximum,
                },
                _ => request
                    .retry_classifier
                    .classify(original, &request.retry_policy),
            };
            attempt_ai_error_with_decision(error, &request.model.common.model_ref, decision)
        })?;

        let execution = LocalApiExecutionContext {
            model: &request.model,
            endpoint: &request.endpoint,
            headers: &request.headers,
            retry_policy: &request.retry_policy,
            retry_classifier: request.retry_classifier.as_ref(),
            transport: self.transport.as_ref(),
            cancellation: &cancellation,
            api_key: request.api_key.as_ref(),
        };
        Ok(self.handler.decode_stream(response, &execution))
    }
}

impl LocalChatApi for LocalHttpChatApi {
    fn stream(
        &self,
        request: LocalResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, AiError>> {
        Box::pin(self.execute(request, cancellation))
    }
}

async fn execute_local_transport_attempt(
    transport: &dyn LocalHttpTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) -> Result<LocalHttpResponse, AttemptFailure> {
    let attempt = request.attempt;
    let timeout = request.timeout;
    let attempt_cancellation = cancellation.child();
    let execution = Box::pin(transport.execute(request, attempt_cancellation.clone()));
    let Some(timeout) = timeout else {
        return execution
            .await
            .map_err(|source| AttemptFailure::transport(attempt, source));
    };
    let timer = Box::pin(futures_timer::Delay::new(timeout));
    match select(execution, timer).await {
        Either::Left((result, _)) => {
            result.map_err(|source| AttemptFailure::transport(attempt, source))
        }
        Either::Right(((), _)) => {
            attempt_cancellation.cancel();
            Err(AttemptFailure::Timeout { attempt, timeout })
        }
    }
}

fn middleware_ai_error(error: crate::MiddlewareError, model: &ModelRef) -> AiError {
    AiError::new(AiErrorKind::Internal, error.message).with_model(model.clone())
}

async fn execute_transport_attempt(
    transport: &dyn HttpTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) -> Result<HttpResponse, AttemptFailure> {
    let attempt = request.attempt;
    let timeout = request.timeout;
    let attempt_cancellation = cancellation.child();
    let execution = Box::pin(transport.execute(request, attempt_cancellation.clone()));
    let Some(timeout) = timeout else {
        return execution
            .await
            .map_err(|source| AttemptFailure::transport(attempt, source));
    };
    let timer = Box::pin(futures_timer::Delay::new(timeout));
    match select(execution, timer).await {
        Either::Left((result, _)) => {
            result.map_err(|source| AttemptFailure::transport(attempt, source))
        }
        Either::Right(((), _)) => {
            attempt_cancellation.cancel();
            Err(AttemptFailure::Timeout { attempt, timeout })
        }
    }
}

fn attempt_ai_error(
    error: AttemptFailure,
    model: &ModelRef,
    classifier: &dyn RetryClassifier,
    policy: &RetryPolicy,
) -> AiError {
    let original = error.original();
    let decision = match &error {
        AttemptFailure::RetryDelayTooLong {
            requested, maximum, ..
        } => RetryDecision::RejectServerDelay {
            requested: *requested,
            maximum: *maximum,
        },
        _ => classifier.classify(original, policy),
    };
    attempt_ai_error_with_decision(error, model, decision)
}

fn attempt_ai_error_with_decision(
    error: AttemptFailure,
    model: &ModelRef,
    decision: RetryDecision,
) -> AiError {
    let original = error.original();
    let kind = match original {
        AttemptFailure::Cancelled => AiErrorKind::Cancelled,
        AttemptFailure::Http { status: 401, .. } => AiErrorKind::Authentication,
        AttemptFailure::Http { status: 403, .. } => AiErrorKind::Authorization,
        AttemptFailure::Http { status: 429, .. } => AiErrorKind::RateLimited,
        AttemptFailure::Http { .. } => AiErrorKind::ProviderRejected,
        AttemptFailure::Transport { .. } | AttemptFailure::Timeout { .. } => AiErrorKind::Transport,
        AttemptFailure::Middleware { .. } => AiErrorKind::Internal,
        AttemptFailure::RetryDelayTooLong { .. } => unreachable!("original failure is unwrapped"),
    };
    let (retryable, retry_after) = match decision {
        RetryDecision::DoNotRetry => (false, None),
        RetryDecision::RetryAfter(delay) => (true, Some(delay)),
        RetryDecision::RejectServerDelay { requested, .. } => (true, Some(requested)),
    };
    let mut result = AiError::new(kind, error.to_string()).with_model(model.clone());
    result.retryable = retryable;
    result.retry_after = retry_after;
    result.status = original.status();
    result.attempt = original.attempt();
    match original {
        AttemptFailure::Http { headers, .. } => {
            result.request_id = request_id_from_headers(headers);
        }
        AttemptFailure::Transport { source, .. } => {
            result.provider_code = Some(source.code.clone());
        }
        AttemptFailure::Timeout { .. } => {
            result.provider_code = Some("timeout".into());
        }
        AttemptFailure::Middleware { source, .. } => {
            result.provider_code = Some(source.code.clone());
        }
        AttemptFailure::Cancelled | AttemptFailure::RetryDelayTooLong { .. } => {}
    }
    result
}

/// Complete provider composition registered atomically with [`crate::Models`].
#[derive(Clone)]
pub struct ProviderRegistration {
    /// Provider identity and request defaults.
    pub descriptor: ProviderDescriptor,
    /// Provider-owned request-time authentication.
    pub auth: Arc<dyn AuthResolver>,
    /// Current immutable catalog snapshot.
    pub catalog: Arc<dyn ModelCatalog>,
    /// API-family dispatch table.
    pub apis: HashMap<ApiId, Arc<dyn ChatApi>>,
    /// Provider-default retry policy.
    pub retry_policy: RetryPolicy,
    /// Provider retry classifier override.
    pub retry_classifier: Arc<dyn RetryClassifier>,
}

impl fmt::Debug for ProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistration")
            .field("descriptor", &self.descriptor)
            .field("models", &self.catalog.snapshot().len())
            .field("refreshable", &self.catalog.catalog_source().is_some())
            .field("apis", &self.apis.keys())
            .field("retry_policy", &self.retry_policy)
            .finish_non_exhaustive()
    }
}

impl ProviderRegistration {
    /// Starts a provider builder with anonymous/keyless auth and an empty
    /// static catalog.
    pub fn builder(id: impl Into<ProviderId>) -> ProviderRegistrationBuilder {
        ProviderRegistrationBuilder::new(id)
    }

    pub(crate) fn validate(&self) -> Result<(), ProviderRegistrationError> {
        for model in self.catalog.snapshot().iter() {
            if model.common.model_ref.provider != self.descriptor.id {
                return Err(ProviderRegistrationError::ModelProviderMismatch {
                    registration: self.descriptor.id.clone(),
                    model: model.common.model_ref.clone(),
                });
            }
            let api = model.api.api_id();
            if !self.apis.contains_key(&api) {
                return Err(ProviderRegistrationError::MissingApi {
                    provider: self.descriptor.id.clone(),
                    api,
                });
            }
        }
        Ok(())
    }
}

/// Builder for one complete provider registration.
pub struct ProviderRegistrationBuilder {
    descriptor: ProviderDescriptor,
    auth: Arc<dyn AuthResolver>,
    catalog: Arc<dyn ModelCatalog>,
    catalog_source: Option<Arc<dyn ModelCatalogSource>>,
    preserve_catalog: bool,
    apis: HashMap<ApiId, Arc<dyn ChatApi>>,
    retry_policy: RetryPolicy,
    retry_classifier: Arc<dyn RetryClassifier>,
}

impl ProviderRegistrationBuilder {
    /// Creates an empty provider registration builder.
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            descriptor: ProviderDescriptor::new(id),
            auth: Arc::new(AnonymousAuthResolver),
            catalog: Arc::new(StaticModelCatalog::new(Vec::new())),
            catalog_source: None,
            preserve_catalog: false,
            apis: HashMap::new(),
            retry_policy: RetryPolicy::default(),
            retry_classifier: Arc::new(DefaultRetryClassifier::default()),
        }
    }

    /// Sets the display name.
    pub fn display_name(mut self, display_name: impl Into<String>) -> Self {
        self.descriptor.display_name = display_name.into();
        self
    }

    /// Sets a provider-wide endpoint override.
    pub fn base_url(mut self, base_url: Url) -> Self {
        self.descriptor.base_url = Some(base_url);
        self
    }

    /// Sets provider/API default headers.
    pub fn headers(mut self, headers: HeaderMapSpec) -> Self {
        self.descriptor.headers = headers;
        self
    }

    /// Sets provider-owned authentication.
    pub fn auth(mut self, auth: Arc<dyn AuthResolver>) -> Self {
        self.auth = auth;
        self
    }

    /// Sets the provider catalog.
    pub fn catalog(mut self, catalog: Arc<dyn ModelCatalog>) -> Self {
        self.catalog = catalog;
        self.catalog_source = None;
        self.preserve_catalog = true;
        self
    }

    /// Sets a dynamic catalog source and uses its baseline as the initial
    /// synchronous snapshot.
    pub fn catalog_source(mut self, source: Arc<dyn ModelCatalogSource>) -> Self {
        self.catalog_source = Some(source);
        self.preserve_catalog = false;
        self
    }

    /// Sets a static catalog directly.
    pub fn models(mut self, models: Vec<ModelDescriptor>) -> Self {
        self.catalog = Arc::new(StaticModelCatalog::new(models));
        self.catalog_source = None;
        self.preserve_catalog = false;
        self
    }

    /// Registers one API implementation under an open API identifier.
    pub fn api(mut self, api: impl Into<ApiId>, implementation: Arc<dyn ChatApi>) -> Self {
        self.apis.insert(api.into(), implementation);
        self
    }

    /// Sets provider-default retry policy.
    pub fn retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Sets a provider-specific retry classifier.
    pub fn retry_classifier(mut self, retry_classifier: Arc<dyn RetryClassifier>) -> Self {
        self.retry_classifier = retry_classifier;
        self
    }

    /// Validates and creates an atomic provider registration.
    pub fn build(self) -> Result<ProviderRegistration, ProviderRegistrationError> {
        let Self {
            descriptor,
            auth,
            catalog,
            catalog_source,
            preserve_catalog,
            apis,
            retry_policy,
            retry_classifier,
        } = self;
        let catalog = if preserve_catalog {
            catalog
        } else {
            let baseline = catalog_source
                .as_ref()
                .map(|source| source.baseline())
                .unwrap_or_else(|| catalog.snapshot());
            let allowed_apis = Arc::from(apis.keys().cloned().collect::<Vec<_>>());
            let catalog_state = Arc::new(
                ProviderCatalogState::new(descriptor.id.clone(), baseline, allowed_apis).map_err(
                    |error| ProviderRegistrationError::Catalog {
                        provider: descriptor.id.clone(),
                        message: error.message,
                    },
                )?,
            );
            Arc::new(ManagedModelCatalog::new(catalog_state, catalog_source))
                as Arc<dyn ModelCatalog>
        };
        let registration = ProviderRegistration {
            descriptor,
            auth,
            catalog,
            apis,
            retry_policy,
            retry_classifier,
        };
        registration.validate()?;
        Ok(registration)
    }
}

/// Invalid provider registration rejected before publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderRegistrationError {
    /// A catalog model names a different provider.
    ModelProviderMismatch {
        /// Provider being registered.
        registration: ProviderId,
        /// Invalid model reference.
        model: ModelRef,
    },
    /// A catalog model has no registered API implementation.
    MissingApi {
        /// Provider being registered.
        provider: ProviderId,
        /// Missing API identifier.
        api: ApiId,
    },
    /// Catalog baseline or managed state is invalid.
    Catalog {
        /// Provider being registered.
        provider: ProviderId,
        /// Secret-free validation detail.
        message: String,
    },
}

impl fmt::Display for ProviderRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelProviderMismatch {
                registration,
                model,
            } => write!(
                formatter,
                "model {model:?} cannot be registered under provider {registration}"
            ),
            Self::MissingApi { provider, api } => {
                write!(
                    formatter,
                    "provider {provider} has no API implementation for {api}"
                )
            }
            Self::Catalog { provider, message } => {
                write!(
                    formatter,
                    "invalid catalog for provider {provider}: {message}"
                )
            }
        }
    }
}

impl std::error::Error for ProviderRegistrationError {}

/// Complete single-threaded provider composition registered atomically with
/// [`crate::LocalModels`]. Every behavior component may retain `Rc` state.
#[derive(Clone)]
pub struct LocalProviderRegistration {
    /// Provider identity and request defaults.
    pub descriptor: ProviderDescriptor,
    /// Provider-owned local request authentication.
    pub auth: Rc<dyn LocalAuthResolver>,
    /// Current immutable local catalog snapshot.
    pub catalog: Rc<dyn LocalModelCatalog>,
    /// Local API-family dispatch table.
    pub apis: HashMap<ApiId, Rc<dyn LocalChatApi>>,
    /// Provider-default retry policy.
    pub retry_policy: RetryPolicy,
    /// Provider local retry classifier override.
    pub retry_classifier: Rc<dyn LocalRetryClassifier>,
}

impl fmt::Debug for LocalProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalProviderRegistration")
            .field("descriptor", &self.descriptor)
            .field("models", &self.catalog.snapshot().len())
            .field("refreshable", &self.catalog.catalog_source().is_some())
            .field("apis", &self.apis.keys())
            .field("retry_policy", &self.retry_policy)
            .finish_non_exhaustive()
    }
}

impl LocalProviderRegistration {
    /// Starts a local provider builder with anonymous auth and an empty static
    /// catalog.
    pub fn builder(id: impl Into<ProviderId>) -> LocalProviderRegistrationBuilder {
        LocalProviderRegistrationBuilder::new(id)
    }

    pub(crate) fn validate(&self) -> Result<(), ProviderRegistrationError> {
        for model in self.catalog.snapshot().iter() {
            if model.common.model_ref.provider != self.descriptor.id {
                return Err(ProviderRegistrationError::ModelProviderMismatch {
                    registration: self.descriptor.id.clone(),
                    model: model.common.model_ref.clone(),
                });
            }
            let api = model.api.api_id();
            if !self.apis.contains_key(&api) {
                return Err(ProviderRegistrationError::MissingApi {
                    provider: self.descriptor.id.clone(),
                    api,
                });
            }
        }
        Ok(())
    }
}

/// Builder for one complete local provider registration.
pub struct LocalProviderRegistrationBuilder {
    descriptor: ProviderDescriptor,
    auth: Rc<dyn LocalAuthResolver>,
    catalog: Rc<dyn LocalModelCatalog>,
    catalog_source: Option<Rc<dyn LocalModelCatalogSource>>,
    preserve_catalog: bool,
    apis: HashMap<ApiId, Rc<dyn LocalChatApi>>,
    retry_policy: RetryPolicy,
    retry_classifier: Rc<dyn LocalRetryClassifier>,
}

impl LocalProviderRegistrationBuilder {
    /// Creates an empty local provider registration builder.
    pub fn new(id: impl Into<ProviderId>) -> Self {
        Self {
            descriptor: ProviderDescriptor::new(id),
            auth: Rc::new(AnonymousAuthResolver),
            catalog: Rc::new(LocalStaticModelCatalog::new(Vec::new())),
            catalog_source: None,
            preserve_catalog: false,
            apis: HashMap::new(),
            retry_policy: RetryPolicy::default(),
            retry_classifier: Rc::new(LocalDefaultRetryClassifier::default()),
        }
    }

    /// Sets the display name.
    pub fn display_name(mut self, display_name: impl Into<String>) -> Self {
        self.descriptor.display_name = display_name.into();
        self
    }

    /// Sets a provider-wide endpoint override.
    pub fn base_url(mut self, base_url: Url) -> Self {
        self.descriptor.base_url = Some(base_url);
        self
    }

    /// Sets provider/API default headers.
    pub fn headers(mut self, headers: HeaderMapSpec) -> Self {
        self.descriptor.headers = headers;
        self
    }

    /// Sets provider-owned local authentication.
    pub fn auth(mut self, auth: Rc<dyn LocalAuthResolver>) -> Self {
        self.auth = auth;
        self
    }

    /// Sets the local provider catalog.
    pub fn catalog(mut self, catalog: Rc<dyn LocalModelCatalog>) -> Self {
        self.catalog = catalog;
        self.catalog_source = None;
        self.preserve_catalog = true;
        self
    }

    /// Sets a non-`Send` dynamic catalog source and uses its baseline as the
    /// initial synchronous snapshot.
    pub fn catalog_source(mut self, source: Rc<dyn LocalModelCatalogSource>) -> Self {
        self.catalog_source = Some(source);
        self.preserve_catalog = false;
        self
    }

    /// Sets a static local catalog directly.
    pub fn models(mut self, models: Vec<ModelDescriptor>) -> Self {
        self.catalog = Rc::new(LocalStaticModelCatalog::new(models));
        self.catalog_source = None;
        self.preserve_catalog = false;
        self
    }

    /// Registers one local API implementation.
    pub fn api(mut self, api: impl Into<ApiId>, implementation: Rc<dyn LocalChatApi>) -> Self {
        self.apis.insert(api.into(), implementation);
        self
    }

    /// Sets provider-default retry policy.
    pub fn retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Sets a provider-specific local retry classifier.
    pub fn retry_classifier(mut self, retry_classifier: Rc<dyn LocalRetryClassifier>) -> Self {
        self.retry_classifier = retry_classifier;
        self
    }

    /// Validates and creates an atomic local provider registration.
    pub fn build(self) -> Result<LocalProviderRegistration, ProviderRegistrationError> {
        let Self {
            descriptor,
            auth,
            catalog,
            catalog_source,
            preserve_catalog,
            apis,
            retry_policy,
            retry_classifier,
        } = self;
        let catalog = if preserve_catalog {
            catalog
        } else {
            let baseline = catalog_source
                .as_ref()
                .map(|source| source.baseline())
                .unwrap_or_else(|| catalog.snapshot());
            let allowed_apis = Rc::from(apis.keys().cloned().collect::<Vec<_>>());
            let catalog_state = Rc::new(
                LocalProviderCatalogState::new(descriptor.id.clone(), baseline, allowed_apis)
                    .map_err(|error| ProviderRegistrationError::Catalog {
                        provider: descriptor.id.clone(),
                        message: error.message,
                    })?,
            );
            Rc::new(LocalManagedModelCatalog::new(catalog_state, catalog_source))
                as Rc<dyn LocalModelCatalog>
        };
        let registration = LocalProviderRegistration {
            descriptor,
            auth,
            catalog,
            apis,
            retry_policy,
            retry_classifier,
        };
        registration.validate()?;
        Ok(registration)
    }
}

/// Applies provider/API default headers while preserving explicit deletion
/// markers for later layers.
pub(crate) fn provider_default_headers(
    registration: &ProviderRegistration,
) -> Result<HeaderMap, AiError> {
    let mut headers = HeaderMap::new();
    apply_header_spec(&mut headers, &registration.descriptor.headers)
        .map_err(|error| AiError::new(AiErrorKind::InvalidRequest, error.message))?;
    Ok(headers)
}

/// Applies local provider/API default headers.
pub(crate) fn local_provider_default_headers(
    registration: &LocalProviderRegistration,
) -> Result<HeaderMap, AiError> {
    let mut headers = HeaderMap::new();
    apply_header_spec(&mut headers, &registration.descriptor.headers)
        .map_err(|error| AiError::new(AiErrorKind::InvalidRequest, error.message))?;
    Ok(headers)
}
