//! Provider registration, API dispatch, and HTTP request establishment from
//! Architecture v2 part 1 §3.5–§3.6 as revised by part 2 §2.4–§2.6 and §3.2.

#![allow(
    clippy::result_large_err,
    reason = "AiError retains the architecture-specified provider and ModelRef fields"
)]

use crate::{
    ApiId, ApiRequestOptions, AssistantStream, AttemptFailure, AttemptMiddleware, AuthError,
    AuthResolutionOverrides, CancellationToken, Context, DefaultRetryClassifier,
    ErasedApiFullOptions, ErasedApiOptionsPatch, ErasedPayloadContext, ErasedPayloadTransform,
    HeaderMapSpec, HttpRequest, HttpResponse, HttpTransport, LocalAssistantStream,
    LocalAttemptMiddleware, LocalBoxFuture, LocalDefaultRetryClassifier, LocalDefaultRetrySleeper,
    LocalErasedPayloadTransform, LocalHttpResponse, LocalHttpTransport, LocalManagedModelCatalog,
    LocalModelCatalogSource, LocalProviderCatalogState, LocalResponseObserver,
    LocalRetryClassifier, LocalRetrySleeper, ManagedModelCatalog, ModelCatalogSource,
    ModelDescriptor, ModelRef, PayloadTransformDisposition, ProviderCatalogState, ProviderId,
    ProviderPayload, ProviderResponseMetadata, RequestStartError, RequestStartErrorKind,
    ResponseObservationContext, ResponseObserver, RetryClassifier, RetryDecision, RetryPolicy,
    RetrySleeper, SecretString, SendBoxFuture, SimpleGenerationOptions, apply_header_spec,
    establish_with_retry_and_local_sleeper, establish_with_retry_and_sleeper,
    request_id_from_headers,
};
use futures_util::{
    StreamExt,
    future::{Either, select},
};
use http::HeaderMap;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use url::Url;

/// Maximum bytes retained while normalizing a provider failure body.
pub const MAX_PROVIDER_ERROR_BODY_BYTES: usize = 64 * 1024;

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
    /// Credential-derived transport invariants that are not logical request
    /// headers. Specialized SDK transports may consume this private channel;
    /// values here never participate in model, explicit, or transform header
    /// precedence.
    pub transport_headers: HeaderMap,
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
            .field("transport_headers", &"<redacted transport invariants>")
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
    /// Network-free configuration inspection for [`crate::Models::check_auth`].
    ConfigurationCheck,
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

/// Non-secret result of checking whether provider auth is configured.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthCheck {
    /// Human-readable source label for account/status UI.
    pub source: Option<AuthSource>,
    /// Configured credential category.
    pub credential_type: crate::CredentialType,
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
                transport_headers: HeaderMap::new(),
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
                transport_headers: HeaderMap::new(),
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
    /// Fully API-specific options, present only for `Models::stream_api`.
    pub full_options: Option<ErasedApiFullOptions>,
    /// Common transport controls for either simple or full execution.
    pub request_options: ApiRequestOptions,
    /// Effective endpoint after auth resolution.
    pub endpoint: Url,
    /// Final logical headers after all header transforms.
    pub headers: HeaderMap,
    /// Credential-derived and API-option-derived invariant headers retained
    /// independently of later overlays for specialized transports.
    pub auth_headers: HeaderMap,
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

/// Original call-option representation retained through response decoding.
#[derive(Clone, Copy)]
pub enum ApiCallOptions<'a> {
    /// Provider-neutral simple options plus their single erased API patch.
    Simple(&'a SimpleGenerationOptions),
    /// Fully typed API-family options.
    Full(&'a ErasedApiFullOptions),
}

impl fmt::Debug for ApiCallOptions<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Simple(_) => formatter.write_str("ApiCallOptions::Simple(<redacted>)"),
            Self::Full(options) => formatter
                .debug_tuple("ApiCallOptions::Full")
                .field(&options.api)
                .finish(),
        }
    }
}

/// API execution resources available during lowering, transport, and decoding.
pub struct ApiExecutionContext<'a> {
    /// Current catalog model.
    pub model: &'a ModelDescriptor,
    /// Canonical request context retained for response-decoder configuration.
    pub context: &'a Context,
    /// Effective endpoint.
    pub endpoint: &'a Url,
    /// Final logical headers.
    pub headers: &'a HeaderMap,
    /// Credential-derived invariant headers retained ahead of caller overlays.
    /// Specialized SDK transports may use these to carry non-wire request
    /// configuration without trusting mutable logical headers.
    pub auth_headers: &'a HeaderMap,
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
    /// Common transport controls, including logical header deletion markers
    /// that are intentionally absent from the finalized [`HeaderMap`].
    pub request_options: &'a ApiRequestOptions,
    /// Original call options, unaffected by payload middleware.
    pub call_options: ApiCallOptions<'a>,
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

    /// Encodes fully API-specific options without invoking simple lowering.
    fn encode_full(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        let _ = context;
        let _ = execution;
        Err(AiError::new(
            AiErrorKind::InvalidRequest,
            format!(
                "API handler {} does not support fully typed options for {}",
                self.api_id(),
                options.api
            ),
        )
        .with_model(model.common.model_ref.clone()))
    }

    /// Projects request-scoped full options needed while resolving provider
    /// authentication. This runs before auth and does not lower or encode the
    /// API request.
    fn apply_full_options_auth_overrides(
        &self,
        _model: &ModelDescriptor,
        _options: &ErasedApiFullOptions,
        _overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Adds API-family defaults derived from fully typed options before model
    /// and explicit request headers are applied.
    fn apply_full_options_headers(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _options: &ErasedApiFullOptions,
        _effective_base_url: &Url,
        _request_options: &ApiRequestOptions,
        _headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Reasserts API-family invariants after every logical payload transform.
    fn finalize_payload(
        &self,
        _payload: &mut ProviderPayload,
        _execution: &ApiExecutionContext<'_>,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Converts an established response body into normalized assistant events.
    fn decode_stream(
        &self,
        response: ProviderResponseStream,
        execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream;
}

fn validate_erased_api_patch(
    request_api: &ApiId,
    handler_api: &ApiId,
    patch: Option<&ErasedApiOptionsPatch>,
    model: &ModelRef,
) -> Result<(), AiError> {
    let Some(patch) = patch else {
        return Ok(());
    };
    if patch.api == *request_api && patch.api == *handler_api {
        return Ok(());
    }

    Err(AiError::new(
        AiErrorKind::InvalidRequest,
        format!(
            "API options for {} cannot be applied to request API {} handled by {}",
            patch.api, request_api, handler_api
        ),
    )
    .with_model(model.clone()))
}

fn validate_erased_full_options(
    request_api: &ApiId,
    handler_api: &ApiId,
    options: &ErasedApiFullOptions,
    model: &ModelRef,
) -> Result<(), AiError> {
    if options.api == *request_api && options.api == *handler_api {
        return Ok(());
    }

    Err(AiError::new(
        AiErrorKind::InvalidRequest,
        format!(
            "full API options for {} cannot be applied to request API {} handled by {}",
            options.api, request_api, handler_api
        ),
    )
    .with_model(model.clone()))
}

/// Provider/API dispatch unit. Concrete HTTP APIs can use [`HttpChatApi`]; SDK
/// and non-HTTP APIs implement this trait directly.
pub trait ChatApi: Send + Sync + 'static {
    /// Projects request-scoped full options needed before provider auth.
    fn apply_full_options_auth_overrides(
        &self,
        _model: &ModelDescriptor,
        _options: &ErasedApiFullOptions,
        _overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Adds full-options-dependent API headers before Models applies the model
    /// and explicit request overlays.
    fn apply_full_options_headers(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _options: &ErasedApiFullOptions,
        _effective_base_url: &Url,
        _request_options: &ApiRequestOptions,
        _headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Adds provider-contextual headers after the model header overlay and
    /// before explicit request headers and the final header transforms.
    fn apply_contextual_headers(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _effective_base_url: &Url,
        _headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        Ok(())
    }

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
            context: &request.context,
            endpoint: &request.endpoint,
            headers: &request.headers,
            auth_headers: &request.auth_headers,
            retry_policy: &request.retry_policy,
            retry_classifier: request.retry_classifier.as_ref(),
            transport: self.transport.as_ref(),
            cancellation: &cancellation,
            api_key: request.api_key.as_ref(),
            request_options: &request.request_options,
            call_options: request.full_options.as_ref().map_or(
                ApiCallOptions::Simple(&request.options),
                ApiCallOptions::Full,
            ),
        };
        let mut payload = if let Some(full_options) = request.full_options.as_ref() {
            validate_erased_full_options(
                &request.api,
                self.handler.api_id(),
                full_options,
                &request.model.common.model_ref,
            )?;
            self.handler
                .encode_full(&request.model, &request.context, full_options, &execution)?
        } else {
            validate_erased_api_patch(
                &request.api,
                self.handler.api_id(),
                request.options.api_options.as_ref(),
                &request.model.common.model_ref,
            )?;
            self.handler.lower_and_encode(
                &request.model,
                &request.context,
                &request.options,
                request.options.api_options.as_ref(),
                &execution,
            )?
        };

        let payload_context = ErasedPayloadContext {
            model: &request.model.common.model_ref,
            api: &request.api,
            endpoint: &request.endpoint,
            headers: &request.headers,
        };
        let transport_session_id = payload.transport_session_id().map(str::to_owned);
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

        self.handler.finalize_payload(&mut payload, &execution)?;

        let body = payload
            .encode_body()
            .map_err(|error| middleware_ai_error(error, &request.model.common.model_ref))?;

        let frozen = HttpRequest {
            method: payload.method,
            url: request.endpoint.clone(),
            headers: request.headers.clone(),
            auth_headers: request.auth_headers.clone(),
            session_id: transport_session_id,
            body,
            timeout: request.timeout,
            transport: request.request_options.transport,
            websocket_connect_timeout: request
                .request_options
                .websocket_connect_timeout_ms
                .map(Duration::from_millis),
            attempt: 0,
        };

        let attempt_middleware = Arc::clone(&request.attempt_middleware);
        let response_observers = Arc::clone(&request.response_observers);
        let model_ref = request.model.common.model_ref.clone();
        let api_id = request.api.clone();
        let endpoint = request.endpoint.clone();
        let transport = Arc::clone(&self.transport);
        let retry_diagnostics = Arc::new(Mutex::new(Vec::new()));
        let mut response = establish_with_retry_and_sleeper(
            &request.retry_policy,
            request.retry_classifier.as_ref(),
            self.sleeper.as_ref(),
            &cancellation,
            |attempt| {
                let mut attempt_request = frozen.clone();
                let invariant_headers = frozen.auth_headers.clone();
                let invariant_session_id = frozen.session_id.clone();
                let cancellation = cancellation.clone();
                let attempt_middleware = Arc::clone(&attempt_middleware);
                let response_observers = Arc::clone(&response_observers);
                let model_ref = model_ref.clone();
                let api_id = api_id.clone();
                let endpoint = endpoint.clone();
                let transport = Arc::clone(&transport);
                let retry_diagnostics = Arc::clone(&retry_diagnostics);
                async move {
                    attempt_request.attempt = attempt;
                    for middleware in attempt_middleware.iter() {
                        middleware
                            .before_attempt(attempt, &mut attempt_request)
                            .await
                            .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                    }
                    attempt_request.auth_headers = invariant_headers;
                    attempt_request.session_id = invariant_session_id;

                    let mut response = match execute_transport_attempt(
                        transport.as_ref(),
                        attempt_request,
                        cancellation.clone(),
                    )
                    .await
                    {
                        Ok(response) => response,
                        Err(mut failure) => {
                            if let AttemptFailure::Transport { source, .. } = &mut failure {
                                retry_diagnostics
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .extend(std::mem::take(&mut source.diagnostics));
                            }
                            return Err(failure);
                        }
                    };
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
                    if response.notify_observers {
                        for observer in response_observers.iter() {
                            observer
                                .on_response(observation, &metadata)
                                .await
                                .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                        }
                    }
                    if !(200..300).contains(&response.status) && !response.decode_non_success {
                        retry_diagnostics
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .extend(std::mem::take(&mut response.diagnostics));
                        return Err(send_http_failure(attempt, response, &cancellation).await);
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
                &request_error_secret_values(
                    &request.headers,
                    &request.auth_headers,
                    request.api_key.as_ref(),
                ),
            )
        })?;
        let mut diagnostics = retry_diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !diagnostics.is_empty() {
            diagnostics.append(&mut response.diagnostics);
            response.diagnostics = std::mem::take(&mut *diagnostics);
        }
        sanitize_send_body_errors(
            &mut response,
            request_error_secret_values(
                &request.headers,
                &request.auth_headers,
                request.api_key.as_ref(),
            ),
        );

        let execution = ApiExecutionContext {
            model: &request.model,
            context: &request.context,
            endpoint: &request.endpoint,
            headers: &request.headers,
            auth_headers: &request.auth_headers,
            retry_policy: &request.retry_policy,
            retry_classifier: request.retry_classifier.as_ref(),
            transport: self.transport.as_ref(),
            cancellation: &cancellation,
            api_key: request.api_key.as_ref(),
            request_options: &request.request_options,
            call_options: request.full_options.as_ref().map_or(
                ApiCallOptions::Simple(&request.options),
                ApiCallOptions::Full,
            ),
        };
        Ok(self.handler.decode_stream(response, &execution))
    }
}

impl ChatApi for HttpChatApi {
    fn apply_full_options_auth_overrides(
        &self,
        model: &ModelDescriptor,
        options: &ErasedApiFullOptions,
        overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        self.handler
            .apply_full_options_auth_overrides(model, options, overrides)
    }

    fn apply_full_options_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        effective_base_url: &Url,
        request_options: &ApiRequestOptions,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        self.handler.apply_full_options_headers(
            model,
            context,
            options,
            effective_base_url,
            request_options,
            headers,
        )
    }

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
    /// Fully API-specific options, present only for `LocalModels::stream_api`.
    pub full_options: Option<ErasedApiFullOptions>,
    /// Common transport controls for either simple or full execution.
    pub request_options: ApiRequestOptions,
    /// Effective endpoint after auth resolution.
    pub endpoint: Url,
    /// Final logical headers.
    pub headers: HeaderMap,
    /// Credential-derived and API-option-derived invariant headers retained
    /// independently of later overlays.
    pub auth_headers: HeaderMap,
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
    /// Canonical request context retained for response-decoder configuration.
    pub context: &'a Context,
    /// Effective endpoint.
    pub endpoint: &'a Url,
    /// Final logical headers.
    pub headers: &'a HeaderMap,
    /// Credential-derived invariant headers retained ahead of caller overlays.
    pub auth_headers: &'a HeaderMap,
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
    /// Common transport controls, including logical header deletion markers
    /// that are intentionally absent from the finalized [`HeaderMap`].
    pub request_options: &'a ApiRequestOptions,
    /// Original call options, unaffected by payload middleware.
    pub call_options: ApiCallOptions<'a>,
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

    /// Encodes fully API-specific options without invoking simple lowering.
    fn encode_full(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        let _ = context;
        let _ = execution;
        Err(AiError::new(
            AiErrorKind::InvalidRequest,
            format!(
                "local API handler {} does not support fully typed options for {}",
                self.api_id(),
                options.api
            ),
        )
        .with_model(model.common.model_ref.clone()))
    }

    /// Local counterpart to
    /// [`ErasedApiHandler::apply_full_options_auth_overrides`].
    fn apply_full_options_auth_overrides(
        &self,
        _model: &ModelDescriptor,
        _options: &ErasedApiFullOptions,
        _overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Adds API-family defaults derived from fully typed options.
    fn apply_full_options_headers(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _options: &ErasedApiFullOptions,
        _effective_base_url: &Url,
        _request_options: &ApiRequestOptions,
        _headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Reasserts API-family invariants after local payload transforms.
    fn finalize_payload(
        &self,
        _payload: &mut ProviderPayload,
        _execution: &LocalApiExecutionContext<'_>,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Converts an established local response into normalized assistant events.
    fn decode_stream(
        &self,
        response: LocalProviderResponseStream,
        execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream;
}

/// Single-threaded provider/API dispatch unit.
pub trait LocalChatApi: 'static {
    /// Projects request-scoped full options needed before local provider auth.
    fn apply_full_options_auth_overrides(
        &self,
        _model: &ModelDescriptor,
        _options: &ErasedApiFullOptions,
        _overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Adds full-options-dependent local API headers before later overlays.
    fn apply_full_options_headers(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _options: &ErasedApiFullOptions,
        _effective_base_url: &Url,
        _request_options: &ApiRequestOptions,
        _headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        Ok(())
    }

    /// Adds provider-contextual local headers after the model header overlay
    /// and before explicit request headers and final header transforms.
    fn apply_contextual_headers(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _effective_base_url: &Url,
        _headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        Ok(())
    }

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
            context: &request.context,
            endpoint: &request.endpoint,
            headers: &request.headers,
            auth_headers: &request.auth_headers,
            retry_policy: &request.retry_policy,
            retry_classifier: request.retry_classifier.as_ref(),
            transport: self.transport.as_ref(),
            cancellation: &cancellation,
            api_key: request.api_key.as_ref(),
            request_options: &request.request_options,
            call_options: request.full_options.as_ref().map_or(
                ApiCallOptions::Simple(&request.options),
                ApiCallOptions::Full,
            ),
        };
        let mut payload = if let Some(full_options) = request.full_options.as_ref() {
            validate_erased_full_options(
                &request.api,
                self.handler.api_id(),
                full_options,
                &request.model.common.model_ref,
            )?;
            self.handler
                .encode_full(&request.model, &request.context, full_options, &execution)?
        } else {
            validate_erased_api_patch(
                &request.api,
                self.handler.api_id(),
                request.options.api_options.as_ref(),
                &request.model.common.model_ref,
            )?;
            self.handler.lower_and_encode(
                &request.model,
                &request.context,
                &request.options,
                request.options.api_options.as_ref(),
                &execution,
            )?
        };

        let payload_context = ErasedPayloadContext {
            model: &request.model.common.model_ref,
            api: &request.api,
            endpoint: &request.endpoint,
            headers: &request.headers,
        };
        let transport_session_id = payload.transport_session_id().map(str::to_owned);
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

        self.handler.finalize_payload(&mut payload, &execution)?;

        let body = payload
            .encode_body()
            .map_err(|error| middleware_ai_error(error, &request.model.common.model_ref))?;
        let frozen = HttpRequest {
            method: payload.method,
            url: request.endpoint.clone(),
            headers: request.headers.clone(),
            auth_headers: request.auth_headers.clone(),
            session_id: transport_session_id,
            body,
            timeout: request.timeout,
            transport: request.request_options.transport,
            websocket_connect_timeout: request
                .request_options
                .websocket_connect_timeout_ms
                .map(Duration::from_millis),
            attempt: 0,
        };

        let attempt_middleware = Rc::clone(&request.attempt_middleware);
        let response_observers = Rc::clone(&request.response_observers);
        let model_ref = request.model.common.model_ref.clone();
        let api_id = request.api.clone();
        let endpoint = request.endpoint.clone();
        let transport = Rc::clone(&self.transport);
        let retry_diagnostics = Rc::new(RefCell::new(Vec::new()));
        let mut response = establish_with_retry_and_local_sleeper(
            &request.retry_policy,
            request.retry_classifier.as_ref(),
            self.sleeper.as_ref(),
            &cancellation,
            |attempt| {
                let mut attempt_request = frozen.clone();
                let invariant_headers = frozen.auth_headers.clone();
                let invariant_session_id = frozen.session_id.clone();
                let cancellation = cancellation.clone();
                let attempt_middleware = Rc::clone(&attempt_middleware);
                let response_observers = Rc::clone(&response_observers);
                let model_ref = model_ref.clone();
                let api_id = api_id.clone();
                let endpoint = endpoint.clone();
                let transport = Rc::clone(&transport);
                let retry_diagnostics = Rc::clone(&retry_diagnostics);
                async move {
                    attempt_request.attempt = attempt;
                    for middleware in attempt_middleware.iter() {
                        middleware
                            .before_attempt(attempt, &mut attempt_request)
                            .await
                            .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                    }
                    attempt_request.auth_headers = invariant_headers;
                    attempt_request.session_id = invariant_session_id;

                    let mut response = match execute_local_transport_attempt(
                        transport.as_ref(),
                        attempt_request,
                        cancellation.clone(),
                    )
                    .await
                    {
                        Ok(response) => response,
                        Err(mut failure) => {
                            if let AttemptFailure::Transport { source, .. } = &mut failure {
                                retry_diagnostics
                                    .borrow_mut()
                                    .extend(std::mem::take(&mut source.diagnostics));
                            }
                            return Err(failure);
                        }
                    };
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
                    if response.notify_observers {
                        for observer in response_observers.iter() {
                            observer
                                .on_response(observation, &metadata)
                                .await
                                .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                        }
                    }
                    if !(200..300).contains(&response.status) && !response.decode_non_success {
                        retry_diagnostics
                            .borrow_mut()
                            .extend(std::mem::take(&mut response.diagnostics));
                        return Err(local_http_failure(attempt, response, &cancellation).await);
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
            attempt_ai_error_with_decision(
                error,
                &request.model.common.model_ref,
                decision,
                &request_error_secret_values(
                    &request.headers,
                    &request.auth_headers,
                    request.api_key.as_ref(),
                ),
            )
        })?;
        let mut diagnostics = retry_diagnostics.borrow_mut();
        if !diagnostics.is_empty() {
            diagnostics.append(&mut response.diagnostics);
            response.diagnostics = std::mem::take(&mut *diagnostics);
        }
        sanitize_local_body_errors(
            &mut response,
            request_error_secret_values(
                &request.headers,
                &request.auth_headers,
                request.api_key.as_ref(),
            ),
        );

        let execution = LocalApiExecutionContext {
            model: &request.model,
            context: &request.context,
            endpoint: &request.endpoint,
            headers: &request.headers,
            auth_headers: &request.auth_headers,
            retry_policy: &request.retry_policy,
            retry_classifier: request.retry_classifier.as_ref(),
            transport: self.transport.as_ref(),
            cancellation: &cancellation,
            api_key: request.api_key.as_ref(),
            request_options: &request.request_options,
            call_options: request.full_options.as_ref().map_or(
                ApiCallOptions::Simple(&request.options),
                ApiCallOptions::Full,
            ),
        };
        Ok(self.handler.decode_stream(response, &execution))
    }
}

impl LocalChatApi for LocalHttpChatApi {
    fn apply_full_options_auth_overrides(
        &self,
        model: &ModelDescriptor,
        options: &ErasedApiFullOptions,
        overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        self.handler
            .apply_full_options_auth_overrides(model, options, overrides)
    }

    fn apply_full_options_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        effective_base_url: &Url,
        request_options: &ApiRequestOptions,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        self.handler.apply_full_options_headers(
            model,
            context,
            options,
            effective_base_url,
            request_options,
            headers,
        )
    }

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

async fn local_http_failure(
    attempt: u32,
    response: LocalHttpResponse,
    cancellation: &CancellationToken,
) -> AttemptFailure {
    let LocalHttpResponse {
        status,
        headers,
        diagnostics: _,
        notify_observers: _,
        decode_non_success: _,
        mut body,
    } = response;
    let bytes = match read_provider_failure_body(&mut body, cancellation).await {
        Ok(bytes) => bytes,
        Err(ErrorBodyReadError::Cancelled) => return AttemptFailure::Cancelled,
        Err(ErrorBodyReadError::Transport(source)) => {
            return AttemptFailure::transport(attempt, source);
        }
    };
    AttemptFailure::http_at(
        attempt,
        status,
        headers,
        SystemTime::now(),
        provider_failure_text(&bytes),
    )
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

async fn send_http_failure(
    attempt: u32,
    response: HttpResponse,
    cancellation: &CancellationToken,
) -> AttemptFailure {
    let HttpResponse {
        status,
        headers,
        diagnostics: _,
        notify_observers: _,
        decode_non_success: _,
        mut body,
    } = response;
    let bytes = match read_provider_failure_body(&mut body, cancellation).await {
        Ok(bytes) => bytes,
        Err(ErrorBodyReadError::Cancelled) => return AttemptFailure::Cancelled,
        Err(ErrorBodyReadError::Transport(source)) => {
            return AttemptFailure::transport(attempt, source);
        }
    };
    AttemptFailure::http_at(
        attempt,
        status,
        headers,
        SystemTime::now(),
        provider_failure_text(&bytes),
    )
}

fn provider_failure_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

enum ErrorBodyReadError {
    Cancelled,
    Transport(crate::TransportError),
}

async fn read_provider_failure_body<S>(
    body: &mut S,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, ErrorBodyReadError>
where
    S: futures_core::Stream<Item = Result<Vec<u8>, crate::TransportError>> + Unpin + ?Sized,
{
    let mut bytes = Vec::new();
    while bytes.len() < MAX_PROVIDER_ERROR_BODY_BYTES {
        if cancellation.is_cancelled() {
            return Err(ErrorBodyReadError::Cancelled);
        }
        let next = Box::pin(body.next());
        let cancelled = Box::pin(cancellation.cancelled());
        let chunk = match select(next, cancelled).await {
            Either::Left((chunk, _)) => chunk,
            Either::Right(((), _)) => return Err(ErrorBodyReadError::Cancelled),
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(ErrorBodyReadError::Transport)?;
        let remaining = MAX_PROVIDER_ERROR_BODY_BYTES - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(bytes)
}

fn attempt_ai_error(
    error: AttemptFailure,
    model: &ModelRef,
    classifier: &dyn RetryClassifier,
    policy: &RetryPolicy,
    secret_values: &[String],
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
    attempt_ai_error_with_decision(error, model, decision, secret_values)
}

fn attempt_ai_error_with_decision(
    error: AttemptFailure,
    model: &ModelRef,
    decision: RetryDecision,
    secret_values: &[String],
) -> AiError {
    let original = error.original();
    let kind = match (original, original.status()) {
        (AttemptFailure::Cancelled, None) => AiErrorKind::Cancelled,
        (_, Some(401)) => AiErrorKind::Authentication,
        (_, Some(403)) => AiErrorKind::Authorization,
        (_, Some(429)) => AiErrorKind::RateLimited,
        (AttemptFailure::Http { .. } | AttemptFailure::Transport { .. }, Some(_)) => {
            AiErrorKind::ProviderRejected
        }
        (AttemptFailure::Transport { .. } | AttemptFailure::Timeout { .. }, None) => {
            AiErrorKind::Transport
        }
        (AttemptFailure::Middleware { .. }, None) => AiErrorKind::Internal,
        (AttemptFailure::Http { .. }, None) => {
            unreachable!("HTTP failures always retain a status")
        }
        (AttemptFailure::Cancelled, Some(_)) => {
            unreachable!("cancelled failures do not retain a status")
        }
        (AttemptFailure::Middleware { .. } | AttemptFailure::Timeout { .. }, Some(_)) => {
            unreachable!("middleware and timeout failures do not retain a status")
        }
        (AttemptFailure::RetryDelayTooLong { .. }, _) => {
            unreachable!("original failure is unwrapped")
        }
    };
    let (retryable, retry_after) = match decision {
        RetryDecision::DoNotRetry => (false, None),
        RetryDecision::RetryAfter(delay) => (true, Some(delay)),
        RetryDecision::RejectServerDelay { requested, .. } => (true, Some(requested)),
    };
    let secret_values = secret_values.iter().map(String::as_str).collect::<Vec<_>>();
    let message = crate::sanitization::redact_public_text(error.to_string(), &secret_values);
    let mut result = AiError::new(kind, message).with_model(model.clone());
    result.retryable = retryable;
    result.retry_after = retry_after;
    result.status = original.status();
    result.attempt = original.attempt();
    match original {
        AttemptFailure::Http { headers, .. } => {
            result.request_id = request_id_from_headers(headers);
        }
        AttemptFailure::Transport { source, .. } => {
            result.provider_code = source
                .provider_code
                .clone()
                .or_else(|| Some(source.code.clone()));
            result.request_id.clone_from(&source.request_id);
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

fn request_error_secret_values(
    headers: &HeaderMap,
    auth_headers: &HeaderMap,
    api_key: Option<&SecretString>,
) -> Vec<String> {
    let mut values = auth_headers
        .values()
        .filter_map(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    values.extend(
        headers
            .iter()
            .filter(|(name, _)| sensitive_header_name(name.as_str()))
            .filter_map(|(_, value)| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    );
    if let Some(api_key) = api_key
        && !api_key.expose_secret().is_empty()
    {
        values.push(api_key.expose_secret().to_owned());
    }
    values.sort_unstable();
    values.dedup();
    values
}

fn sanitize_send_body_errors(response: &mut crate::HttpResponse, secret_values: Vec<String>) {
    let body = std::mem::replace(&mut response.body, Box::pin(futures_util::stream::empty()));
    response.body =
        Box::pin(body.map(move |item| item.map_err(|error| error.sanitized(&secret_values))));
}

fn sanitize_local_body_errors(response: &mut crate::LocalHttpResponse, secret_values: Vec<String>) {
    let body = std::mem::replace(&mut response.body, Box::pin(futures_util::stream::empty()));
    response.body =
        Box::pin(body.map(move |item| item.map_err(|error| error.sanitized(&secret_values))));
}

fn sensitive_header_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cf-aig-authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "api-key"
            | "cookie"
            | "set-cookie"
    )
}

/// Credential-scoped model filter used by provider registrations.
pub type ModelAvailabilityFilter = dyn Fn(&[ModelDescriptor], Option<&crate::Credential>) -> Vec<ModelDescriptor>
    + Send
    + Sync
    + 'static;

/// Local-executor credential-scoped model filter.
pub type LocalModelAvailabilityFilter =
    dyn Fn(&[ModelDescriptor], Option<&crate::Credential>) -> Vec<ModelDescriptor> + 'static;

/// Complete provider composition registered atomically with [`crate::Models`].
#[derive(Clone)]
pub struct ProviderRegistration {
    /// Provider identity and request defaults.
    pub descriptor: ProviderDescriptor,
    /// Provider-owned request-time authentication.
    pub auth: Arc<dyn AuthResolver>,
    /// Current immutable catalog snapshot.
    pub catalog: Arc<dyn ModelCatalog>,
    /// Optional provider policy that narrows the complete catalog for one
    /// stored credential.
    pub filter_models: Option<Arc<ModelAvailabilityFilter>>,
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
            .field("credential_scoped", &self.filter_models.is_some())
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
    filter_models: Option<Arc<ModelAvailabilityFilter>>,
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
            filter_models: None,
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

    /// Sets a credential-scoped model-availability policy. The provider's
    /// synchronous catalog remains complete.
    pub fn filter_models(mut self, filter: Arc<ModelAvailabilityFilter>) -> Self {
        self.filter_models = Some(filter);
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
            filter_models,
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
            filter_models,
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
    /// Optional local credential-scoped model policy.
    pub filter_models: Option<Rc<LocalModelAvailabilityFilter>>,
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
            .field("credential_scoped", &self.filter_models.is_some())
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
    filter_models: Option<Rc<LocalModelAvailabilityFilter>>,
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
            filter_models: None,
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

    /// Sets a local credential-scoped model-availability policy.
    pub fn filter_models(mut self, filter: Rc<LocalModelAvailabilityFilter>) -> Self {
        self.filter_models = Some(filter);
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
            filter_models,
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
            filter_models,
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
