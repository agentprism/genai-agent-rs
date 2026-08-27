//! Google Vertex credential resolution.

use agentprism_ai::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyResolveRequest, AuthAnswer, AuthContext, AuthError,
    AuthEvent, AuthInfoLink, AuthInteraction, AuthPrompt, AuthResolutionPurpose, AuthResolver,
    AuthSelectOption, AuthSource, CancellationToken, Credential, HttpRequest, HttpTransport,
    LocalApiKeyAuth, LocalApiKeyResolveRequest, LocalAuthContext, LocalAuthInteraction,
    LocalAuthResolver, LocalBoxFuture, LocalHttpTransport, LocalProviderAuthResolver,
    LocalResolveAuthRequest, ProviderAuthResolver, ResolveAuthRequest, ResolvedAuth, SecretString,
    SendBoxFuture,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{FutureExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, header};
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey as _;
use rsa::signature::{SignatureEncoding as _, Signer as _};
use serde::Deserialize;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

/// Sentinel used by pinned Pi to select Application Default Credentials rather
/// than an explicit Vertex API key.
pub const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";

/// Pinned Pi's host-visible fallback ADC path.
pub const VERTEX_ADC_PATH: &str = "~/.config/gcloud/application_default_credentials.json";

/// OAuth scope that `@google/genai` requires for Vertex ADC.
pub const VERTEX_CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// GoogleAuth credential families whose token acquisition depends on
/// host/platform capabilities beyond a portable HTTP exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VertexAdcCredentialType {
    /// Workload or workforce identity federation credentials.
    ExternalAccount,
    /// Workforce identity federation authorized-user credentials.
    ExternalAccountAuthorizedUser,
    /// Service-account impersonation credentials with nested source auth.
    ImpersonatedServiceAccount,
    /// Google Distributed Cloud Hosted service-account credentials.
    GdchServiceAccount,
}

impl VertexAdcCredentialType {
    fn from_google_type(value: &str) -> Option<Self> {
        match value {
            "external_account" => Some(Self::ExternalAccount),
            "external_account_authorized_user" => Some(Self::ExternalAccountAuthorizedUser),
            "impersonated_service_account" => Some(Self::ImpersonatedServiceAccount),
            "gdch_service_account" => Some(Self::GdchServiceAccount),
            _ => None,
        }
    }

    /// Returns the credential-file discriminator consumed by GoogleAuth.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExternalAccount => "external_account",
            Self::ExternalAccountAuthorizedUser => "external_account_authorized_user",
            Self::ImpersonatedServiceAccount => "impersonated_service_account",
            Self::GdchServiceAccount => "gdch_service_account",
        }
    }
}

/// Secret-bearing request delegated to a host GoogleAuth integration.
#[derive(Clone)]
pub struct VertexAdcTokenRequest {
    /// Credential path supplied to pinned Pi's `googleAuthOptions.keyFilename`.
    pub credential_path: String,
    /// GoogleAuth credential family selected from the validated top-level type.
    pub credential_type: VertexAdcCredentialType,
    /// Exact UTF-8 credential document read through the host auth context.
    pub credential_json: SecretString,
    /// Required Vertex OAuth scopes.
    pub scopes: Vec<String>,
    /// Effective quota project selected with GoogleAuth precedence: the
    /// `GOOGLE_CLOUD_QUOTA_PROJECT` override, then credential metadata.
    pub quota_project_id: Option<String>,
}

impl std::fmt::Debug for VertexAdcTokenRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VertexAdcTokenRequest")
            .field("credential_path", &self.credential_path)
            .field("credential_type", &self.credential_type)
            .field("credential_json", &self.credential_json)
            .field("scopes", &self.scopes)
            .field("quota_project_id", &self.quota_project_id)
            .finish()
    }
}

/// Host adapter for ADC formats implemented by GoogleAuth but requiring
/// platform identity, nested delegation, CA, metadata, or executable support.
///
/// Implementations must validate the selected credential family before using
/// endpoints or local sources from the document. This is the portable seam for
/// a host binding backed by `google-auth-library`, a platform Google auth SDK,
/// or an equivalently validated credential implementation.
pub trait VertexAdcCredentialAdapter: Send + Sync + 'static {
    /// Resolves one bearer token for a supported ADC credential document.
    fn resolve_access_token(
        &self,
        request: VertexAdcTokenRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<SecretString, AuthError>>;
}

/// Local-executor counterpart to [`VertexAdcCredentialAdapter`].
pub trait LocalVertexAdcCredentialAdapter: 'static {
    /// Resolves one bearer token for a supported ADC credential document.
    fn resolve_access_token(
        &self,
        request: VertexAdcTokenRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<SecretString, AuthError>>;
}

/// Creates the Send Vertex auth resolver.
pub fn google_vertex_auth_resolver(transport: Arc<dyn HttpTransport>) -> Arc<dyn AuthResolver> {
    Arc::new(GoogleVertexAuthResolver {
        inner: ProviderAuthResolver::new(
            Some(Arc::new(VertexApiKeyAuth {
                transport,
                adc_adapter: None,
            })),
            None,
        ),
    })
}

/// Creates the Send Vertex auth resolver with host support for delegated ADC
/// credential families such as external-account and impersonated credentials.
pub fn google_vertex_auth_resolver_with_adc_adapter(
    transport: Arc<dyn HttpTransport>,
    adc_adapter: Arc<dyn VertexAdcCredentialAdapter>,
) -> Arc<dyn AuthResolver> {
    Arc::new(GoogleVertexAuthResolver {
        inner: ProviderAuthResolver::new(
            Some(Arc::new(VertexApiKeyAuth {
                transport,
                adc_adapter: Some(adc_adapter),
            })),
            None,
        ),
    })
}

/// Creates the local-executor Vertex auth resolver.
pub fn local_google_vertex_auth_resolver(
    transport: Rc<dyn LocalHttpTransport>,
) -> Rc<dyn LocalAuthResolver> {
    Rc::new(LocalGoogleVertexAuthResolver {
        inner: LocalProviderAuthResolver::new(
            Some(Rc::new(LocalVertexApiKeyAuth {
                transport,
                adc_adapter: None,
            })),
            None,
        ),
    })
}

/// Creates the local Vertex auth resolver with host support for delegated ADC
/// credential families such as external-account and impersonated credentials.
pub fn local_google_vertex_auth_resolver_with_adc_adapter(
    transport: Rc<dyn LocalHttpTransport>,
    adc_adapter: Rc<dyn LocalVertexAdcCredentialAdapter>,
) -> Rc<dyn LocalAuthResolver> {
    Rc::new(LocalGoogleVertexAuthResolver {
        inner: LocalProviderAuthResolver::new(
            Some(Rc::new(LocalVertexApiKeyAuth {
                transport,
                adc_adapter: Some(adc_adapter),
            })),
            None,
        ),
    })
}

struct GoogleVertexAuthResolver {
    inner: ProviderAuthResolver,
}

impl AuthResolver for GoogleVertexAuthResolver {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            if request.purpose == AuthResolutionPurpose::ConfigurationCheck {
                let credential = request
                    .credential_store
                    .read(request.provider.id.clone(), cancellation.clone())
                    .await?;
                return check_vertex_send(credential, request.auth_context, cancellation).await;
            }
            let custom_base_url = custom_model_base_url(
                request.model.as_ref(),
                "https://us-central1-aiplatform.googleapis.com",
            );
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            if resolved.api_key.as_ref().is_some_and(is_real_api_key) {
                insert_google_api_key_header(&mut resolved)?;
            }
            if custom_base_url.is_some() {
                resolved.base_url = custom_base_url;
            }
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

struct LocalGoogleVertexAuthResolver {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalGoogleVertexAuthResolver {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            if request.purpose == AuthResolutionPurpose::ConfigurationCheck {
                let credential = request
                    .credential_store
                    .read(request.provider.id.clone(), cancellation.clone())
                    .await?;
                return check_vertex_local(credential, request.auth_context, cancellation).await;
            }
            let custom_base_url = custom_model_base_url(
                request.model.as_ref(),
                "https://us-central1-aiplatform.googleapis.com",
            );
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            if resolved.api_key.as_ref().is_some_and(is_real_api_key) {
                insert_google_api_key_header(&mut resolved)?;
            }
            if custom_base_url.is_some() {
                resolved.base_url = custom_base_url;
            }
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

async fn check_vertex_send(
    credential: Option<Credential>,
    auth_context: Arc<dyn AuthContext>,
    cancellation: CancellationToken,
) -> Result<Option<ResolvedAuth>, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let credential = match credential.as_ref() {
        Some(Credential::ApiKey(credential)) => Some(credential),
        Some(Credential::OAuth(_)) => return Ok(None),
        None => None,
    };
    if let Some(key) = credential.and_then(|credential| credential.key.as_ref())
        && is_real_api_key(key)
    {
        return Ok(Some(vertex_auth_check("stored credential")));
    }
    let environment = BTreeMap::new();
    let ambient_key = send_env(
        &environment,
        auth_context.as_ref(),
        "GOOGLE_CLOUD_API_KEY",
        cancellation.clone(),
    )
    .await?
    .map(SecretString::new);
    if ambient_key.as_ref().is_some_and(is_real_api_key) {
        return Ok(Some(vertex_auth_check("GOOGLE_CLOUD_API_KEY")));
    }

    let adc_path = send_credential_env(
        credential,
        &environment,
        auth_context.as_ref(),
        "GOOGLE_APPLICATION_CREDENTIALS",
        cancellation.clone(),
    )
    .await?
    .unwrap_or_else(|| VERTEX_ADC_PATH.to_owned());
    if !auth_context
        .file_exists(adc_path, cancellation.clone())
        .await?
    {
        return Ok(None);
    }
    let project = send_credential_env(
        credential,
        &environment,
        auth_context.as_ref(),
        "GOOGLE_CLOUD_PROJECT",
        cancellation.clone(),
    )
    .await?
    .or(send_env(
        &environment,
        auth_context.as_ref(),
        "GCLOUD_PROJECT",
        cancellation.clone(),
    )
    .await?);
    let location = send_credential_env(
        credential,
        &environment,
        auth_context.as_ref(),
        "GOOGLE_CLOUD_LOCATION",
        cancellation,
    )
    .await?;
    if project.as_deref().is_some_and(|value| !value.is_empty())
        && location.as_deref().is_some_and(|value| !value.is_empty())
    {
        return Ok(Some(vertex_auth_check(if credential.is_some() {
            "stored credential"
        } else {
            "gcloud application default credentials"
        })));
    }
    Ok(None)
}

async fn check_vertex_local(
    credential: Option<Credential>,
    auth_context: Rc<dyn LocalAuthContext>,
    cancellation: CancellationToken,
) -> Result<Option<ResolvedAuth>, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let credential = match credential.as_ref() {
        Some(Credential::ApiKey(credential)) => Some(credential),
        Some(Credential::OAuth(_)) => return Ok(None),
        None => None,
    };
    if let Some(key) = credential.and_then(|credential| credential.key.as_ref())
        && is_real_api_key(key)
    {
        return Ok(Some(vertex_auth_check("stored credential")));
    }
    let environment = BTreeMap::new();
    let ambient_key = local_env(
        &environment,
        auth_context.as_ref(),
        "GOOGLE_CLOUD_API_KEY",
        cancellation.clone(),
    )
    .await?
    .map(SecretString::new);
    if ambient_key.as_ref().is_some_and(is_real_api_key) {
        return Ok(Some(vertex_auth_check("GOOGLE_CLOUD_API_KEY")));
    }

    let adc_path = local_credential_env(
        credential,
        &environment,
        auth_context.as_ref(),
        "GOOGLE_APPLICATION_CREDENTIALS",
        cancellation.clone(),
    )
    .await?
    .unwrap_or_else(|| VERTEX_ADC_PATH.to_owned());
    if !auth_context
        .file_exists(adc_path, cancellation.clone())
        .await?
    {
        return Ok(None);
    }
    let project = local_credential_env(
        credential,
        &environment,
        auth_context.as_ref(),
        "GOOGLE_CLOUD_PROJECT",
        cancellation.clone(),
    )
    .await?
    .or(local_env(
        &environment,
        auth_context.as_ref(),
        "GCLOUD_PROJECT",
        cancellation.clone(),
    )
    .await?);
    let location = local_credential_env(
        credential,
        &environment,
        auth_context.as_ref(),
        "GOOGLE_CLOUD_LOCATION",
        cancellation,
    )
    .await?;
    if project.as_deref().is_some_and(|value| !value.is_empty())
        && location.as_deref().is_some_and(|value| !value.is_empty())
    {
        return Ok(Some(vertex_auth_check(if credential.is_some() {
            "stored credential"
        } else {
            "gcloud application default credentials"
        })));
    }
    Ok(None)
}

fn vertex_auth_check(source: &str) -> ResolvedAuth {
    ResolvedAuth {
        api_key: None,
        headers: HeaderMap::new(),
        transport_headers: HeaderMap::new(),
        environment: std::collections::BTreeMap::new(),
        base_url: None,
        source: AuthSource::new(source),
    }
}

fn insert_google_api_key_header(resolved: &mut ResolvedAuth) -> Result<(), AuthError> {
    let Some(secret) = resolved.api_key.as_ref() else {
        return Ok(());
    };
    let value = HeaderValue::from_str(secret.expose_secret()).map_err(|_| {
        AuthError::new(
            "invalid_api_key",
            "credential cannot be encoded as a header",
        )
    })?;
    resolved.headers.insert("x-goog-api-key", value);
    Ok(())
}

fn custom_model_base_url(
    model: Option<&agentprism_ai::ModelDescriptor>,
    catalog_default: &str,
) -> Option<Url> {
    let base_url = model?.common.base_url.clone();
    let default = Url::parse(catalog_default).expect("static catalog base URL");
    (base_url != default).then_some(base_url)
}

fn is_real_api_key(secret: &SecretString) -> bool {
    let value = secret.expose_secret().trim();
    !value.is_empty() && value != GCP_VERTEX_CREDENTIALS_MARKER && !is_placeholder_api_key(value)
}

fn is_placeholder_api_key(value: &str) -> bool {
    value
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
        .is_some_and(|value| {
            !value.is_empty()
                && !value
                    .chars()
                    .any(|character| matches!(character, '>' | '\r' | '\n'))
        })
}

fn vertex_api_key_base_url() -> Url {
    Url::parse("https://aiplatform.googleapis.com/v1").expect("static Vertex API-key endpoint URL")
}

#[derive(Clone)]
struct VertexApiKeyAuth {
    transport: Arc<dyn HttpTransport>,
    adc_adapter: Option<Arc<dyn VertexAdcCredentialAdapter>>,
}

impl std::fmt::Debug for VertexApiKeyAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VertexApiKeyAuth")
            .finish_non_exhaustive()
    }
}

impl ApiKeyAuth for VertexApiKeyAuth {
    fn name(&self) -> &str {
        "Google Cloud credentials"
    }

    fn resolve(
        &self,
        request: ApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            resolve_vertex_send(
                request,
                self.transport.as_ref(),
                self.adc_adapter.as_deref(),
                cancellation,
            )
            .await
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        Box::pin(async move { login_vertex_send(interaction, cancellation).await })
    }
}

#[derive(Clone)]
struct LocalVertexApiKeyAuth {
    transport: Rc<dyn LocalHttpTransport>,
    adc_adapter: Option<Rc<dyn LocalVertexAdcCredentialAdapter>>,
}

impl std::fmt::Debug for LocalVertexApiKeyAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalVertexApiKeyAuth")
            .finish_non_exhaustive()
    }
}

impl LocalApiKeyAuth for LocalVertexApiKeyAuth {
    fn name(&self) -> &str {
        "Google Cloud credentials"
    }

    fn resolve(
        &self,
        request: LocalApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            resolve_vertex_local(
                request,
                self.transport.as_ref(),
                self.adc_adapter.as_deref(),
                cancellation,
            )
            .await
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        Box::pin(async move { login_vertex_local(interaction, cancellation).await })
    }
}

async fn resolve_vertex_send(
    request: ApiKeyResolveRequest,
    transport: &dyn HttpTransport,
    adc_adapter: Option<&dyn VertexAdcCredentialAdapter>,
    cancellation: CancellationToken,
) -> Result<Option<ResolvedAuth>, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let credential = request.credential.as_ref();
    let key = if let Some(key) = credential.and_then(|credential| credential.key.clone()) {
        Some((key, "stored credential".to_owned()))
    } else {
        send_env(
            &request.environment,
            request.context.as_ref(),
            "GOOGLE_CLOUD_API_KEY",
            cancellation.clone(),
        )
        .await?
        .map(|key| (SecretString::new(key), "GOOGLE_CLOUD_API_KEY".to_owned()))
    };
    if let Some((key, source)) = key {
        let value = key.expose_secret().trim();
        if is_real_api_key(&key) {
            return Ok(Some(ResolvedAuth {
                api_key: Some(SecretString::new(value)),
                headers: HeaderMap::new(),
                transport_headers: HeaderMap::new(),
                environment: credential.map_or_else(std::collections::BTreeMap::new, |value| {
                    value.environment.clone()
                }),
                // The Google SDK uses Vertex Express's global endpoint when
                // an API key is supplied, independently of project/location.
                base_url: Some(vertex_api_key_base_url()),
                source: AuthSource::new(source),
            }));
        }
    }
    let adc_path = send_credential_env(
        credential,
        &request.environment,
        request.context.as_ref(),
        "GOOGLE_APPLICATION_CREDENTIALS",
        cancellation.clone(),
    )
    .await?
    .unwrap_or_else(|| VERTEX_ADC_PATH.to_owned());
    let has_credentials = request
        .context
        .file_exists(adc_path.clone(), cancellation.clone())
        .await?;
    if !has_credentials {
        return Ok(None);
    }
    let project = send_credential_env(
        credential,
        &request.environment,
        request.context.as_ref(),
        "GOOGLE_CLOUD_PROJECT",
        cancellation.clone(),
    )
    .await?
    .or(send_env(
        &request.environment,
        request.context.as_ref(),
        "GCLOUD_PROJECT",
        cancellation.clone(),
    )
    .await?);
    let location = send_credential_env(
        credential,
        &request.environment,
        request.context.as_ref(),
        "GOOGLE_CLOUD_LOCATION",
        cancellation.clone(),
    )
    .await?;
    let (Some(project), Some(location)) = (
        project.filter(|value| !value.is_empty()),
        location.filter(|value| !value.is_empty()),
    ) else {
        return Ok(None);
    };
    let credential_json = request
        .context
        .read_file(adc_path.clone(), cancellation.clone())
        .await?
        .ok_or_else(|| {
            AuthError::new(
                "vertex_adc_unreadable",
                "Vertex ADC exists but the auth host did not provide its credential bytes",
            )
        })?;
    let quota_project_id_override = send_env(
        &request.environment,
        request.context.as_ref(),
        "GOOGLE_CLOUD_QUOTA_PROJECT",
        cancellation.clone(),
    )
    .await?;
    let adc = resolve_adc_token_send(
        adc_path,
        credential_json,
        quota_project_id_override,
        transport,
        adc_adapter,
        cancellation.clone(),
    )
    .await?;
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    finish_vertex_resolution(Some(project), Some(location), credential.is_some(), adc)
}

async fn send_credential_env(
    credential: Option<&ApiKeyCredential>,
    overrides: &BTreeMap<String, String>,
    context: &dyn agentprism_ai::AuthContext,
    name: &str,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = credential
        .and_then(|credential| credential.environment.get(name))
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(value.clone()));
    }
    send_env(overrides, context, name, cancellation).await
}

async fn send_env(
    overrides: &BTreeMap<String, String>,
    context: &dyn agentprism_ai::AuthContext,
    name: &str,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = overrides.get(name).filter(|value| !value.is_empty()) {
        return Ok(Some(value.clone()));
    }
    context.env(name.to_owned(), cancellation).await
}

async fn resolve_vertex_local(
    request: LocalApiKeyResolveRequest,
    transport: &dyn LocalHttpTransport,
    adc_adapter: Option<&dyn LocalVertexAdcCredentialAdapter>,
    cancellation: CancellationToken,
) -> Result<Option<ResolvedAuth>, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let credential = request.credential.as_ref();
    let key = if let Some(key) = credential.and_then(|credential| credential.key.clone()) {
        Some((key, "stored credential".to_owned()))
    } else {
        local_env(
            &request.environment,
            request.context.as_ref(),
            "GOOGLE_CLOUD_API_KEY",
            cancellation.clone(),
        )
        .await?
        .map(|key| (SecretString::new(key), "GOOGLE_CLOUD_API_KEY".to_owned()))
    };
    if let Some((key, source)) = key {
        let value = key.expose_secret().trim();
        if is_real_api_key(&key) {
            return Ok(Some(ResolvedAuth {
                api_key: Some(SecretString::new(value)),
                headers: HeaderMap::new(),
                transport_headers: HeaderMap::new(),
                environment: credential.map_or_else(std::collections::BTreeMap::new, |value| {
                    value.environment.clone()
                }),
                base_url: Some(vertex_api_key_base_url()),
                source: AuthSource::new(source),
            }));
        }
    }
    let adc_path = local_credential_env(
        credential,
        &request.environment,
        request.context.as_ref(),
        "GOOGLE_APPLICATION_CREDENTIALS",
        cancellation.clone(),
    )
    .await?
    .unwrap_or_else(|| VERTEX_ADC_PATH.to_owned());
    let has_credentials = request
        .context
        .file_exists(adc_path.clone(), cancellation.clone())
        .await?;
    if !has_credentials {
        return Ok(None);
    }
    let project = local_credential_env(
        credential,
        &request.environment,
        request.context.as_ref(),
        "GOOGLE_CLOUD_PROJECT",
        cancellation.clone(),
    )
    .await?
    .or(local_env(
        &request.environment,
        request.context.as_ref(),
        "GCLOUD_PROJECT",
        cancellation.clone(),
    )
    .await?);
    let location = local_credential_env(
        credential,
        &request.environment,
        request.context.as_ref(),
        "GOOGLE_CLOUD_LOCATION",
        cancellation.clone(),
    )
    .await?;
    let (Some(project), Some(location)) = (
        project.filter(|value| !value.is_empty()),
        location.filter(|value| !value.is_empty()),
    ) else {
        return Ok(None);
    };
    let credential_json = request
        .context
        .read_file(adc_path.clone(), cancellation.clone())
        .await?
        .ok_or_else(|| {
            AuthError::new(
                "vertex_adc_unreadable",
                "Vertex ADC exists but the auth host did not provide its credential bytes",
            )
        })?;
    let quota_project_id_override = local_env(
        &request.environment,
        request.context.as_ref(),
        "GOOGLE_CLOUD_QUOTA_PROJECT",
        cancellation.clone(),
    )
    .await?;
    let adc = resolve_adc_token_local(
        adc_path,
        credential_json,
        quota_project_id_override,
        transport,
        adc_adapter,
        cancellation.clone(),
    )
    .await?;
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    finish_vertex_resolution(Some(project), Some(location), credential.is_some(), adc)
}

async fn local_credential_env(
    credential: Option<&ApiKeyCredential>,
    overrides: &BTreeMap<String, String>,
    context: &dyn agentprism_ai::LocalAuthContext,
    name: &str,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = credential
        .and_then(|credential| credential.environment.get(name))
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(value.clone()));
    }
    local_env(overrides, context, name, cancellation).await
}

async fn local_env(
    overrides: &BTreeMap<String, String>,
    context: &dyn agentprism_ai::LocalAuthContext,
    name: &str,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = overrides.get(name).filter(|value| !value.is_empty()) {
        return Ok(Some(value.clone()));
    }
    context.env(name.to_owned(), cancellation).await
}

#[derive(Deserialize)]
struct GoogleAdcDocument {
    #[serde(rename = "type")]
    credential_type: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    client_email: Option<String>,
    private_key: Option<String>,
    quota_project_id: Option<String>,
}

#[derive(Deserialize)]
struct GoogleAccessTokenResponse {
    access_token: String,
}

enum AdcTokenExchange {
    Refresh { url: Url, body: Vec<u8> },
    Delegate(VertexAdcCredentialType),
}

struct AdcTokenPlan {
    exchange: AdcTokenExchange,
    quota_project_id: Option<String>,
}

struct ResolvedVertexAdc {
    access_token: SecretString,
    quota_project_id: Option<String>,
}

fn adc_token_plan(bytes: &[u8]) -> Result<AdcTokenPlan, AuthError> {
    let document: GoogleAdcDocument = serde_json::from_slice(bytes).map_err(|_| {
        AuthError::new(
            "invalid_vertex_adc",
            "Vertex ADC credential file is not valid JSON",
        )
    })?;
    let credential_type = document.credential_type.clone();
    let quota_project_id = document
        .quota_project_id
        .clone()
        .filter(|value| !value.is_empty());
    let exchange = if credential_type.as_deref() == Some("authorized_user") {
        authorized_user_token_plan(document)?
    } else if let Some(credential_type) = credential_type
        .as_deref()
        .and_then(VertexAdcCredentialType::from_google_type)
    {
        AdcTokenExchange::Delegate(credential_type)
    } else {
        // google-auth-library dispatches every remaining document to its JWT
        // client, including a missing or unknown `type`, then validates the
        // service-account fields. In particular, an arbitrary `access_token`
        // property is never accepted as a credential.
        service_account_token_plan(document)?
    };

    Ok(AdcTokenPlan {
        exchange,
        quota_project_id,
    })
}

fn authorized_user_token_plan(document: GoogleAdcDocument) -> Result<AdcTokenExchange, AuthError> {
    let required = |value: Option<String>, field: &str| {
        value.filter(|value| !value.is_empty()).ok_or_else(|| {
            AuthError::new(
                "invalid_vertex_adc",
                format!("Vertex authorized-user ADC is missing {field}"),
            )
        })
    };
    let client_id = required(document.client_id, "client_id")?;
    let client_secret = required(document.client_secret, "client_secret")?;
    let refresh_token = required(document.refresh_token, "refresh_token")?;
    // UserRefreshClient ignores credential-file token_uri and always uses its
    // fixed OAuth2 endpoint.
    let url =
        Url::parse("https://oauth2.googleapis.com/token").expect("static Google OAuth token URL");
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("client_id", &client_id)
        .append_pair("client_secret", &client_secret)
        .append_pair("refresh_token", &refresh_token)
        .finish()
        .into_bytes();
    Ok(AdcTokenExchange::Refresh { url, body })
}

fn service_account_token_plan(document: GoogleAdcDocument) -> Result<AdcTokenExchange, AuthError> {
    let required = |value: Option<String>, field: &str| {
        value.filter(|value| !value.is_empty()).ok_or_else(|| {
            AuthError::new(
                "invalid_vertex_adc",
                format!("Vertex service-account credential is missing {field}"),
            )
        })
    };
    let client_email = required(document.client_email, "client_email")?;
    let private_key = required(document.private_key, "private_key")?;
    let url =
        Url::parse("https://oauth2.googleapis.com/token").expect("static Google OAuth token URL");
    let assertion = service_account_assertion(&client_email, &private_key)?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
        .append_pair("assertion", &assertion)
        .finish()
        .into_bytes();
    Ok(AdcTokenExchange::Refresh { url, body })
}

fn service_account_assertion(client_email: &str, private_key: &str) -> Result<String, AuthError> {
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            AuthError::new(
                "invalid_vertex_adc",
                "system clock is before the Unix epoch",
            )
        })?
        .as_secs();
    let expires_at = issued_at.checked_add(3_600).ok_or_else(|| {
        AuthError::new(
            "invalid_vertex_adc",
            "system clock cannot represent the service-account token expiry",
        )
    })?;
    let header = br#"{"alg":"RS256"}"#;
    let claims = serde_json::to_vec(&serde_json::json!({
        "iss": client_email,
        "scope": VERTEX_CLOUD_PLATFORM_SCOPE,
        "aud": "https://oauth2.googleapis.com/token",
        "exp": expires_at,
        "iat": issued_at,
    }))
    .map_err(|_| {
        AuthError::new(
            "invalid_vertex_adc",
            "service-account claims could not be encoded",
        )
    })?;
    let header = URL_SAFE_NO_PAD.encode(header);
    let claims = URL_SAFE_NO_PAD.encode(claims);
    let signing_input = format!("{header}.{claims}");
    let key = RsaPrivateKey::from_pkcs8_pem(private_key)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(private_key))
        .map_err(|_| {
            AuthError::new(
                "invalid_vertex_adc",
                "Vertex service-account private_key is not a valid RSA PEM key",
            )
        })?;
    let signature = SigningKey::<Sha256>::new(key).sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_vec())
    ))
}

fn adc_token_request(url: Url, body: Vec<u8>) -> Result<HttpRequest, AuthError> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    Ok(HttpRequest {
        method: Method::POST,
        url,
        headers,
        auth_headers: HeaderMap::new(),
        session_id: None,
        body,
        timeout: None,
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    })
}

fn parse_adc_token_response(status: u16, bytes: &[u8]) -> Result<SecretString, AuthError> {
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "vertex_adc_exchange_failed",
            format!("Vertex ADC token exchange returned HTTP {status}"),
        ));
    }
    let response: GoogleAccessTokenResponse = serde_json::from_slice(bytes).map_err(|_| {
        AuthError::new(
            "invalid_vertex_adc_response",
            "Vertex ADC token response is not valid JSON",
        )
    })?;
    if response.access_token.trim().is_empty() {
        return Err(AuthError::new(
            "invalid_vertex_adc_response",
            "Vertex ADC token response omitted access_token",
        ));
    }
    Ok(SecretString::new(response.access_token))
}

async fn resolve_adc_token_send(
    credential_path: String,
    credential_json: SecretString,
    quota_project_id_override: Option<String>,
    transport: &dyn HttpTransport,
    adc_adapter: Option<&dyn VertexAdcCredentialAdapter>,
    cancellation: CancellationToken,
) -> Result<ResolvedVertexAdc, AuthError> {
    let plan = adc_token_plan(credential_json.expose_secret().as_bytes())?;
    let quota_project_id = quota_project_id_override.or(plan.quota_project_id);
    let (url, body) = match plan.exchange {
        AdcTokenExchange::Refresh { url, body } => (url, body),
        AdcTokenExchange::Delegate(credential_type) => {
            let Some(adapter) = adc_adapter else {
                return Err(unsupported_adc_adapter(credential_type));
            };
            let token = adapter
                .resolve_access_token(
                    VertexAdcTokenRequest {
                        credential_path,
                        credential_type,
                        credential_json,
                        scopes: vec![VERTEX_CLOUD_PLATFORM_SCOPE.to_owned()],
                        quota_project_id: quota_project_id.clone(),
                    },
                    cancellation.clone(),
                )
                .await?;
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            return Ok(ResolvedVertexAdc {
                access_token: validate_adc_access_token(token)?,
                quota_project_id,
            });
        }
    };
    let execute = transport
        .execute(adc_token_request(url, body)?, cancellation.child())
        .fuse();
    let cancelled = cancellation.cancelled().fuse();
    futures_util::pin_mut!(execute, cancelled);
    let mut response = futures_util::select_biased! {
        _ = cancelled => return Err(AuthError::Cancelled),
        response = execute => response.map_err(|_| AuthError::new(
            "vertex_adc_transport",
            "Vertex ADC token exchange transport failed",
        ))?,
    };
    let status = response.status;
    let mut bytes = Vec::new();
    loop {
        let next = response.body.next().fuse();
        let cancelled = cancellation.cancelled().fuse();
        futures_util::pin_mut!(next, cancelled);
        let chunk = futures_util::select_biased! {
            _ = cancelled => return Err(AuthError::Cancelled),
            chunk = next => chunk,
        };
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(|_| {
            AuthError::new(
                "vertex_adc_transport",
                "Vertex ADC token response body failed",
            )
        })?;
        if bytes.len().saturating_add(chunk.len()) > 64 * 1024 {
            return Err(AuthError::new(
                "invalid_vertex_adc_response",
                "Vertex ADC token response exceeded 64 KiB",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(ResolvedVertexAdc {
        access_token: parse_adc_token_response(status, &bytes)?,
        quota_project_id,
    })
}

async fn resolve_adc_token_local(
    credential_path: String,
    credential_json: SecretString,
    quota_project_id_override: Option<String>,
    transport: &dyn LocalHttpTransport,
    adc_adapter: Option<&dyn LocalVertexAdcCredentialAdapter>,
    cancellation: CancellationToken,
) -> Result<ResolvedVertexAdc, AuthError> {
    let plan = adc_token_plan(credential_json.expose_secret().as_bytes())?;
    let quota_project_id = quota_project_id_override.or(plan.quota_project_id);
    let (url, body) = match plan.exchange {
        AdcTokenExchange::Refresh { url, body } => (url, body),
        AdcTokenExchange::Delegate(credential_type) => {
            let Some(adapter) = adc_adapter else {
                return Err(unsupported_adc_adapter(credential_type));
            };
            let token = adapter
                .resolve_access_token(
                    VertexAdcTokenRequest {
                        credential_path,
                        credential_type,
                        credential_json,
                        scopes: vec![VERTEX_CLOUD_PLATFORM_SCOPE.to_owned()],
                        quota_project_id: quota_project_id.clone(),
                    },
                    cancellation.clone(),
                )
                .await?;
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            return Ok(ResolvedVertexAdc {
                access_token: validate_adc_access_token(token)?,
                quota_project_id,
            });
        }
    };
    let execute = transport
        .execute(adc_token_request(url, body)?, cancellation.child())
        .fuse();
    let cancelled = cancellation.cancelled().fuse();
    futures_util::pin_mut!(execute, cancelled);
    let mut response = futures_util::select_biased! {
        _ = cancelled => return Err(AuthError::Cancelled),
        response = execute => response.map_err(|_| AuthError::new(
            "vertex_adc_transport",
            "Vertex ADC token exchange transport failed",
        ))?,
    };
    let status = response.status;
    let mut bytes = Vec::new();
    loop {
        let next = response.body.next().fuse();
        let cancelled = cancellation.cancelled().fuse();
        futures_util::pin_mut!(next, cancelled);
        let chunk = futures_util::select_biased! {
            _ = cancelled => return Err(AuthError::Cancelled),
            chunk = next => chunk,
        };
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(|_| {
            AuthError::new(
                "vertex_adc_transport",
                "Vertex ADC token response body failed",
            )
        })?;
        if bytes.len().saturating_add(chunk.len()) > 64 * 1024 {
            return Err(AuthError::new(
                "invalid_vertex_adc_response",
                "Vertex ADC token response exceeded 64 KiB",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(ResolvedVertexAdc {
        access_token: parse_adc_token_response(status, &bytes)?,
        quota_project_id,
    })
}

fn unsupported_adc_adapter(credential_type: VertexAdcCredentialType) -> AuthError {
    AuthError::new(
        "unsupported_vertex_adc_type",
        format!(
            "Vertex ADC credential type {} requires a host GoogleAuth adapter",
            credential_type.as_str()
        ),
    )
}

fn validate_adc_access_token(token: SecretString) -> Result<SecretString, AuthError> {
    if token.expose_secret().trim().is_empty() {
        return Err(AuthError::new(
            "invalid_vertex_adc_response",
            "Vertex ADC credential adapter omitted access_token",
        ));
    }
    Ok(token)
}

fn finish_vertex_resolution(
    project: Option<String>,
    location: Option<String>,
    stored: bool,
    adc: ResolvedVertexAdc,
) -> Result<Option<ResolvedAuth>, AuthError> {
    let (Some(project), Some(location)) = (project, location) else {
        return Ok(None);
    };
    let host = match location.as_str() {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "us" | "eu" => format!("aiplatform.{location}.rep.googleapis.com"),
        _ => format!("{location}-aiplatform.googleapis.com"),
    };
    let mut endpoint = format!("https://{host}/v1");
    endpoint.push_str("/projects/");
    endpoint.push_str(&project);
    endpoint.push_str("/locations/");
    endpoint.push_str(&location);
    let endpoint = Url::parse(&endpoint).map_err(|_| {
        AuthError::new(
            "invalid_vertex_endpoint",
            "invalid Google Cloud project or location",
        )
    })?;
    let mut headers = HeaderMap::new();
    let authorization =
        HeaderValue::from_str(&format!("Bearer {}", adc.access_token.expose_secret())).map_err(
            |_| AuthError::new("invalid_adc_token", "ADC access token is not header-safe"),
        )?;
    headers.insert(header::AUTHORIZATION, authorization);
    if let Some(quota_project_id) = adc.quota_project_id {
        let value = HeaderValue::from_str(&quota_project_id).map_err(|_| {
            AuthError::new(
                "invalid_vertex_quota_project",
                "Vertex quota project ID is not header-safe",
            )
        })?;
        headers.insert("x-goog-user-project", value);
    }
    Ok(Some(ResolvedAuth {
        api_key: None,
        headers,
        transport_headers: HeaderMap::new(),
        environment: std::collections::BTreeMap::new(),
        base_url: Some(endpoint),
        source: AuthSource::new(if stored {
            "stored credential"
        } else {
            "gcloud application default credentials"
        }),
    }))
}

fn vertex_method_prompt() -> AuthPrompt {
    AuthPrompt::Select {
        message: "Select Google Vertex AI authentication method:".to_owned(),
        options: vec![
            AuthSelectOption {
                id: "api-key".to_owned(),
                label: "Google Cloud API key".to_owned(),
                description: None,
            },
            AuthSelectOption {
                id: "adc".to_owned(),
                label: "Application Default Credentials".to_owned(),
                description: None,
            },
            AuthSelectOption {
                id: "service-account".to_owned(),
                label: "Service account credentials file".to_owned(),
                description: None,
            },
        ],
    }
}

fn vertex_info(method: &str) -> AuthEvent {
    AuthEvent::Info {
        message: if method == "adc" {
            "Run `gcloud auth application-default login`, then provide the project and location."
        } else {
            "Provide a service account credentials file, project, and location."
        }
        .to_owned(),
        links: vec![AuthInfoLink {
            url: Url::parse("https://cloud.google.com/docs/authentication/provide-credentials-adc")
                .expect("static Google documentation URL"),
            label: Some("Application Default Credentials".to_owned()),
        }],
    }
}

fn text_prompt(message: &str, secret: bool) -> AuthPrompt {
    if secret {
        AuthPrompt::Secret {
            message: message.to_owned(),
            placeholder: None,
        }
    } else {
        AuthPrompt::Text {
            message: message.to_owned(),
            placeholder: None,
        }
    }
}

fn text_answer(answer: AuthAnswer, kind: &str) -> Result<String, AuthError> {
    let AuthAnswer::Text(value) = answer else {
        return Err(AuthError::new(
            "invalid_auth_answer",
            format!("{kind} prompt returned a non-text answer"),
        ));
    };
    Ok(value)
}

async fn login_vertex_send(
    interaction: Arc<dyn AuthInteraction>,
    cancellation: CancellationToken,
) -> Result<ApiKeyCredential, AuthError> {
    let method = interaction
        .prompt(vertex_method_prompt(), cancellation.clone())
        .await?;
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let AuthAnswer::Selected(method) = method else {
        return Err(AuthError::new(
            "invalid_auth_answer",
            "Vertex auth selection returned a non-selection answer",
        ));
    };
    if method == "api-key" {
        let key = interaction
            .prompt(
                text_prompt("Enter Google Cloud API key", true),
                cancellation.clone(),
            )
            .await?;
        return Ok(ApiKeyCredential {
            key: Some(SecretString::new(text_answer(key, "secret")?)),
            environment: BTreeMap::new(),
        });
    }
    if !matches!(method.as_str(), "adc" | "service-account") {
        return Err(AuthError::new(
            "invalid_auth_method",
            format!("unknown Google Vertex AI auth method: {method}"),
        ));
    }
    interaction.notify(vertex_info(&method))?;
    let project = text_answer(
        interaction
            .prompt(
                text_prompt("Enter Google Cloud project ID", false),
                cancellation.clone(),
            )
            .await?,
        "text",
    )?;
    let location = text_answer(
        interaction
            .prompt(
                text_prompt("Enter Google Cloud location", false),
                cancellation.clone(),
            )
            .await?,
        "text",
    )?;
    let path = if method == "service-account" {
        Some(text_answer(
            interaction
                .prompt(
                    text_prompt("Enter service account credentials file path", false),
                    cancellation.clone(),
                )
                .await?,
            "text",
        )?)
    } else {
        None
    };
    Ok(vertex_environment(project, location, path))
}

async fn login_vertex_local(
    interaction: Rc<dyn LocalAuthInteraction>,
    cancellation: CancellationToken,
) -> Result<ApiKeyCredential, AuthError> {
    let method = interaction
        .prompt(vertex_method_prompt(), cancellation.clone())
        .await?;
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let AuthAnswer::Selected(method) = method else {
        return Err(AuthError::new(
            "invalid_auth_answer",
            "Vertex auth selection returned a non-selection answer",
        ));
    };
    if method == "api-key" {
        let key = interaction
            .prompt(
                text_prompt("Enter Google Cloud API key", true),
                cancellation.clone(),
            )
            .await?;
        return Ok(ApiKeyCredential {
            key: Some(SecretString::new(text_answer(key, "secret")?)),
            environment: BTreeMap::new(),
        });
    }
    if !matches!(method.as_str(), "adc" | "service-account") {
        return Err(AuthError::new(
            "invalid_auth_method",
            format!("unknown Google Vertex AI auth method: {method}"),
        ));
    }
    interaction.notify(vertex_info(&method))?;
    let project = text_answer(
        interaction
            .prompt(
                text_prompt("Enter Google Cloud project ID", false),
                cancellation.clone(),
            )
            .await?,
        "text",
    )?;
    let location = text_answer(
        interaction
            .prompt(
                text_prompt("Enter Google Cloud location", false),
                cancellation.clone(),
            )
            .await?,
        "text",
    )?;
    let path = if method == "service-account" {
        Some(text_answer(
            interaction
                .prompt(
                    text_prompt("Enter service account credentials file path", false),
                    cancellation.clone(),
                )
                .await?,
            "text",
        )?)
    } else {
        None
    };
    Ok(vertex_environment(project, location, path))
}

fn vertex_environment(
    project: String,
    location: String,
    credentials_path: Option<String>,
) -> ApiKeyCredential {
    let mut environment = BTreeMap::from([
        ("GOOGLE_CLOUD_PROJECT".to_owned(), project),
        ("GOOGLE_CLOUD_LOCATION".to_owned(), location),
    ]);
    if let Some(path) = credentials_path.filter(|path| !path.is_empty()) {
        environment.insert("GOOGLE_APPLICATION_CREDENTIALS".to_owned(), path);
    }
    ApiKeyCredential {
        key: None,
        environment,
    }
}
