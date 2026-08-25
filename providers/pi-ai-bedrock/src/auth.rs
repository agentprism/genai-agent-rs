//! Amazon Bedrock bearer-token, profile, and ambient AWS authentication.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use http::{HeaderMap, HeaderName, HeaderValue, header};
use pi_ai::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyResolveRequest, AuthAnswer, AuthError, AuthEvent,
    AuthInfoLink, AuthInteraction, AuthPrompt, AuthResolver, AuthSelectOption, AuthSource,
    CancellationToken, LocalApiKeyAuth, LocalApiKeyResolveRequest, LocalAuthInteraction,
    LocalAuthResolver, LocalBoxFuture, LocalProviderAuthResolver, LocalResolveAuthRequest,
    ProviderAuthResolver, ResolveAuthRequest, ResolvedAuth, SecretString, SendBoxFuture,
    is_ecmascript_whitespace,
};
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

const BEARER_ENV: &str = "AWS_BEARER_TOKEN_BEDROCK";
const PROFILE_ENV: &str = "AWS_PROFILE";
const SKIP_AUTH_ENV: &str = "AWS_BEDROCK_SKIP_AUTH";
const FORCE_HTTP1_ENV: &str = "AWS_BEDROCK_FORCE_HTTP1";
const FORCE_CACHE_ENV: &str = "AWS_BEDROCK_FORCE_CACHE";
const CACHE_RETENTION_ENV: &str = "PI_CACHE_RETENTION";
const HTTP_PROXY_ENV: &str = "http_proxy";
const HTTPS_PROXY_ENV: &str = "https_proxy";
const ALL_PROXY_ENV: &str = "all_proxy";
const NO_PROXY_ENV: &str = "no_proxy";
pub(crate) const SIGNING_CONFIG_HEADER: HeaderName =
    HeaderName::from_static("x-pi-bedrock-signing-config");

const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE: &str = "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";

/// Static AWS credentials selected for one Bedrock request.
#[derive(Clone, Eq, PartialEq)]
pub struct BedrockStaticCredentials {
    /// AWS access-key identifier.
    pub access_key_id: SecretString,
    /// AWS secret access key.
    pub secret_access_key: SecretString,
    /// Optional temporary-session token.
    pub session_token: Option<SecretString>,
}

impl std::fmt::Debug for BedrockStaticCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BedrockStaticCredentials")
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Complete AWS client/signing configuration resolved for one Bedrock request.
///
/// The provider-specific signer receives this value directly. It intentionally
/// stays outside the provider-neutral persisted model and message schemas.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct BedrockSigningConfig {
    /// Selected shared-configuration profile, if any.
    pub profile: Option<String>,
    /// Final AWS request region after ARN, option, environment, and Pi fallback
    /// precedence.
    pub region: Option<String>,
    /// Explicit SDK endpoint override. `None` deliberately leaves standard
    /// endpoint resolution to the AWS SDK/default profile chain.
    pub endpoint: Option<Url>,
    /// Explicit static credentials. Absence selects the SDK default chain.
    pub credentials: Option<BedrockStaticCredentials>,
    /// Bedrock bearer token. Presence selects AWS `httpBearerAuth` instead of
    /// SigV4.
    pub bearer_token: Option<SecretString>,
    /// Whether Pi's skip-auth compatibility mode selected dummy credentials.
    pub skip_auth: bool,
    /// Whether request options or proxy selection require an HTTP/1.1-capable
    /// Smithy client.
    pub force_http1: bool,
    /// Request-scoped HTTP(S) proxy selected for the model endpoint. A signer
    /// receiving this value must use a proxy-capable HTTP/1 handler.
    pub proxy_url: Option<Url>,
    /// Whether provider environment selected long cache retention by default.
    pub long_cache_retention: bool,
    /// Whether otherwise-unidentified inference profiles receive explicit
    /// cache points.
    pub force_prompt_caching: bool,
    has_scoped_profile: bool,
    has_ambient_profile: bool,
    proxy_environment: BedrockProxyEnvironment,
}

impl std::fmt::Debug for BedrockSigningConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BedrockSigningConfig")
            .field(
                "profile",
                &self.profile.as_ref().map(|_| "[REDACTED PROFILE]"),
            )
            .field("region", &self.region)
            .field(
                "endpoint",
                &self.endpoint.as_ref().map(|_| "[REDACTED ENDPOINT]"),
            )
            .field("credentials", &self.credentials)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("skip_auth", &self.skip_auth)
            .field("force_http1", &self.force_http1)
            .field(
                "proxy_url",
                &self.proxy_url.as_ref().map(|_| "[REDACTED PROXY]"),
            )
            .field("long_cache_retention", &self.long_cache_retention)
            .field("force_prompt_caching", &self.force_prompt_caching)
            .finish()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct BedrockProxyEnvironment {
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    all_proxy: Option<String>,
    no_proxy: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSigningConfig {
    profile: Option<String>,
    region: Option<String>,
    endpoint: Option<Url>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    bearer_token: Option<String>,
    skip_auth: bool,
    force_http1: bool,
    proxy_url: Option<Url>,
    long_cache_retention: bool,
    force_prompt_caching: bool,
    has_scoped_profile: bool,
    has_ambient_profile: bool,
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    all_proxy: Option<String>,
    no_proxy: Option<String>,
}

impl From<&BedrockSigningConfig> for StoredSigningConfig {
    fn from(config: &BedrockSigningConfig) -> Self {
        Self {
            profile: config.profile.clone(),
            region: config.region.clone(),
            endpoint: config.endpoint.clone(),
            access_key_id: config
                .credentials
                .as_ref()
                .map(|value| value.access_key_id.expose_secret().to_owned()),
            secret_access_key: config
                .credentials
                .as_ref()
                .map(|value| value.secret_access_key.expose_secret().to_owned()),
            session_token: config
                .credentials
                .as_ref()
                .and_then(|value| value.session_token.as_ref())
                .map(|value| value.expose_secret().to_owned()),
            bearer_token: config
                .bearer_token
                .as_ref()
                .map(|value| value.expose_secret().to_owned()),
            skip_auth: config.skip_auth,
            force_http1: config.force_http1,
            proxy_url: config.proxy_url.clone(),
            long_cache_retention: config.long_cache_retention,
            force_prompt_caching: config.force_prompt_caching,
            has_scoped_profile: config.has_scoped_profile,
            has_ambient_profile: config.has_ambient_profile,
            http_proxy: config.proxy_environment.http_proxy.clone(),
            https_proxy: config.proxy_environment.https_proxy.clone(),
            all_proxy: config.proxy_environment.all_proxy.clone(),
            no_proxy: config.proxy_environment.no_proxy.clone(),
        }
    }
}

impl StoredSigningConfig {
    fn into_config(self) -> Result<BedrockSigningConfig, AuthError> {
        let credentials = match (self.access_key_id, self.secret_access_key) {
            (Some(access_key_id), Some(secret_access_key)) => Some(BedrockStaticCredentials {
                access_key_id: SecretString::new(access_key_id),
                secret_access_key: SecretString::new(secret_access_key),
                session_token: self.session_token.map(SecretString::new),
            }),
            (None, None) => None,
            _ => {
                return Err(AuthError::new(
                    "invalid_bedrock_signing_config",
                    "Bedrock signing configuration contains an incomplete static credential",
                ));
            }
        };
        Ok(BedrockSigningConfig {
            profile: self.profile,
            region: self.region,
            endpoint: self.endpoint,
            credentials,
            bearer_token: self.bearer_token.map(SecretString::new),
            skip_auth: self.skip_auth,
            force_http1: self.force_http1,
            proxy_url: self.proxy_url,
            long_cache_retention: self.long_cache_retention,
            force_prompt_caching: self.force_prompt_caching,
            has_scoped_profile: self.has_scoped_profile,
            has_ambient_profile: self.has_ambient_profile,
            proxy_environment: BedrockProxyEnvironment {
                http_proxy: self.http_proxy,
                https_proxy: self.https_proxy,
                all_proxy: self.all_proxy,
                no_proxy: self.no_proxy,
            },
        })
    }
}

fn signing_config_header(config: &BedrockSigningConfig) -> Result<HeaderValue, AuthError> {
    let bytes = serde_json::to_vec(&StoredSigningConfig::from(config)).map_err(|error| {
        AuthError::new(
            "invalid_bedrock_signing_config",
            format!("failed to encode Bedrock signing configuration: {error}"),
        )
    })?;
    let mut value = HeaderValue::from_str(&URL_SAFE_NO_PAD.encode(bytes)).map_err(|_| {
        AuthError::new(
            "invalid_bedrock_signing_config",
            "Bedrock signing configuration cannot be carried to the signer",
        )
    })?;
    // Mark this as a transport-only invariant. Models keeps sensitive auth
    // values outside the mutable logical header overlay; provenance therefore
    // remains structural even if a later transform inserts a real clone.
    value.set_sensitive(true);
    Ok(value)
}

pub(crate) fn signing_config_from_headers(
    headers: &HeaderMap,
) -> Result<BedrockSigningConfig, AuthError> {
    let Some(value) = headers.get(&SIGNING_CONFIG_HEADER) else {
        return Ok(BedrockSigningConfig::default());
    };
    let encoded = value.to_str().map_err(|_| {
        AuthError::new(
            "invalid_bedrock_signing_config",
            "Bedrock signing configuration is not an ASCII header value",
        )
    })?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| {
        AuthError::new(
            "invalid_bedrock_signing_config",
            "Bedrock signing configuration is not valid base64url",
        )
    })?;
    serde_json::from_slice::<StoredSigningConfig>(&bytes)
        .map_err(|error| {
            AuthError::new(
                "invalid_bedrock_signing_config",
                format!("invalid Bedrock signing configuration: {error}"),
            )
        })?
        .into_config()
}

fn attach_signing_config(
    resolved: &mut ResolvedAuth,
    config: &BedrockSigningConfig,
) -> Result<(), AuthError> {
    resolved.transport_headers.insert(
        SIGNING_CONFIG_HEADER.clone(),
        signing_config_header(config)?,
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct BedrockApiKeyAuth;

impl ApiKeyAuth for BedrockApiKeyAuth {
    fn name(&self) -> &str {
        "AWS credentials or bearer token"
    }

    fn resolve(
        &self,
        request: ApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move { resolve_send(request, cancellation).await })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        Box::pin(async move {
            let answer = interaction
                .prompt(authentication_method_prompt(), cancellation.clone())
                .await?;
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            let AuthAnswer::Selected(method) = answer else {
                return Err(AuthError::new(
                    "invalid_auth_answer",
                    "Bedrock authentication selection returned a non-selection answer",
                ));
            };
            login_send(interaction, method, cancellation).await
        })
    }
}

impl LocalApiKeyAuth for BedrockApiKeyAuth {
    fn name(&self) -> &str {
        "AWS credentials or bearer token"
    }

    fn resolve(
        &self,
        request: LocalApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move { resolve_local(request, cancellation).await })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        Box::pin(async move {
            let answer = interaction
                .prompt(authentication_method_prompt(), cancellation.clone())
                .await?;
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            let AuthAnswer::Selected(method) = answer else {
                return Err(AuthError::new(
                    "invalid_auth_answer",
                    "Bedrock authentication selection returned a non-selection answer",
                ));
            };
            login_local(interaction, method, cancellation).await
        })
    }
}

struct BedrockAuthResolver {
    inner: ProviderAuthResolver,
}

impl AuthResolver for BedrockAuthResolver {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let routing = request.clone();
            let Some(mut resolved) = self.inner.resolve(request, cancellation.clone()).await?
            else {
                return Ok(None);
            };
            let mut signing = signing_config_from_headers(&resolved.transport_headers)?;
            finalize_signing_config(
                &mut signing,
                routing.model.as_ref().map(|model| {
                    (
                        model.common.model_ref.model.as_str(),
                        &model.common.base_url,
                    )
                }),
            )?;
            resolved.base_url = signing.endpoint.clone();
            attach_signing_config(&mut resolved, &signing)?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

struct LocalBedrockAuthResolver {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalBedrockAuthResolver {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let routing = request.clone();
            let Some(mut resolved) = self.inner.resolve(request, cancellation.clone()).await?
            else {
                return Ok(None);
            };
            let mut signing = signing_config_from_headers(&resolved.transport_headers)?;
            finalize_signing_config(
                &mut signing,
                routing.model.as_ref().map(|model| {
                    (
                        model.common.model_ref.model.as_str(),
                        &model.common.base_url,
                    )
                }),
            )?;
            resolved.base_url = signing.endpoint.clone();
            attach_signing_config(&mut resolved, &signing)?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

pub(crate) fn bedrock_auth_resolver() -> Arc<dyn AuthResolver> {
    Arc::new(BedrockAuthResolver {
        inner: ProviderAuthResolver::new(Some(Arc::new(BedrockApiKeyAuth)), None),
    })
}

pub(crate) fn local_bedrock_auth_resolver() -> Rc<dyn LocalAuthResolver> {
    Rc::new(LocalBedrockAuthResolver {
        inner: LocalProviderAuthResolver::new(Some(Rc::new(BedrockApiKeyAuth)), None),
    })
}

async fn resolve_send(
    request: ApiKeyResolveRequest,
    cancellation: CancellationToken,
) -> Result<Option<ResolvedAuth>, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let credential_key = request
        .credential
        .as_ref()
        .and_then(|credential| credential.key.clone())
        .filter(|key| !key.expose_secret().is_empty());
    let credential_profile = credential_environment_entry(&request.credential, PROFILE_ENV);
    let scoped_profile = credential_profile
        .as_ref()
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| {
            request
                .environment
                .get(PROFILE_ENV)
                .filter(|value| !value.is_empty())
                .cloned()
        });
    // Pinned amazon-bedrock.ts uses nullish coalescing for the stored profile.
    // A present empty string therefore blocks the ambient AWS_PROFILE lookup,
    // but then fails the subsequent JavaScript truthiness check.
    let auth_ambient_profile = if credential_profile.is_some() {
        None
    } else {
        ambient_value_send(&request, PROFILE_ENV, cancellation.clone()).await?
    };
    let auth_profile = scoped_profile
        .clone()
        .or_else(|| auth_ambient_profile.clone());
    // Bedrock client construction performs its own ambient AWS_PROFILE lookup,
    // independently of the provider-auth eligibility check above. In
    // particular, a stored profile suppresses ambient lookup only for auth
    // resolution; ambient profile presence still prevents pinning a standard
    // catalog endpoint or fallback region. An empty stored profile can fail
    // auth eligibility while the client later selects the ambient profile
    // after ambient access keys establish eligibility.
    let client_ambient_profile =
        ambient_value_send(&request, PROFILE_ENV, cancellation.clone()).await?;
    // Pinned provider resolution consults the stored credential environment
    // only for AWS_PROFILE. Bearer/static/role-chain eligibility comes from
    // request overrides or the ambient host context, never arbitrary stored
    // credential fields.
    let environment_bearer = auth_value_send(&request, BEARER_ENV, cancellation.clone()).await?;
    let auth_credentials = static_credentials_send(&request, false, cancellation.clone()).await?;
    let chain_source = default_chain_source_send(&request, cancellation.clone()).await?;
    let source = if credential_key.is_some() {
        Some("stored credential")
    } else if environment_bearer.is_some() {
        Some(BEARER_ENV)
    } else if credential_environment(&request.credential, PROFILE_ENV).is_some() {
        Some("stored credential")
    } else if auth_profile.is_some() {
        Some(PROFILE_ENV)
    } else if auth_credentials.is_some() {
        Some("AWS access keys")
    } else if chain_source.is_some() {
        chain_source
    } else {
        None
    };
    let Some(source) = source else {
        return Ok(None);
    };

    // amazon-bedrock.ts returns a credential environment only when a stored
    // key/profile path owns the provider (or when an existing stored
    // credential accompanies the winning profile path). An ambient bearer
    // result deliberately drops a stored profile before the direct Bedrock API
    // performs client configuration. Explicit request overrides remain a
    // separate input and are never dropped.
    let retain_credential_environment = credential_key.is_some()
        || (environment_bearer.is_none() && auth_profile.is_some() && request.credential.is_some());
    let client_scoped_profile =
        configuration_scoped_value_send(&request, PROFILE_ENV, retain_credential_environment);
    let client_profile = client_scoped_profile
        .clone()
        .or_else(|| client_ambient_profile.clone());
    let configured_region = configuration_value_send(
        &request,
        "AWS_REGION",
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .or(configuration_value_send(
        &request,
        "AWS_DEFAULT_REGION",
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?);
    let skip_auth = configuration_value_send(
        &request,
        SKIP_AUTH_ENV,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .as_deref()
        == Some("1");
    let force_http1 = configuration_value_send(
        &request,
        FORCE_HTTP1_ENV,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .as_deref()
        == Some("1");
    let long_cache_retention = configuration_value_send(
        &request,
        CACHE_RETENTION_ENV,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .as_deref()
        == Some("long");
    let force_prompt_caching = configuration_value_send(
        &request,
        FORCE_CACHE_ENV,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .as_deref()
        == Some("1");
    let proxy_environment = proxy_environment_send(
        &request,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?;
    let bearer_token = (!skip_auth)
        .then(|| {
            credential_key
                .clone()
                .or_else(|| environment_bearer.clone().map(SecretString::new))
        })
        .flatten();
    let credentials = if skip_auth {
        Some(dummy_credentials())
    } else if client_scoped_profile.is_none() {
        static_credentials_send(&request, retain_credential_environment, cancellation).await?
    } else {
        None
    };
    resolved_auth(
        source,
        BedrockSigningConfig {
            profile: client_profile,
            region: configured_region,
            endpoint: None,
            credentials,
            bearer_token,
            skip_auth,
            force_http1,
            proxy_url: None,
            long_cache_retention,
            force_prompt_caching,
            has_scoped_profile: client_scoped_profile.is_some(),
            has_ambient_profile: client_ambient_profile.is_some(),
            proxy_environment,
        },
    )
    .map(Some)
}

async fn resolve_local(
    request: LocalApiKeyResolveRequest,
    cancellation: CancellationToken,
) -> Result<Option<ResolvedAuth>, AuthError> {
    cancellation.check().map_err(|_| AuthError::Cancelled)?;
    let credential_key = request
        .credential
        .as_ref()
        .and_then(|credential| credential.key.clone())
        .filter(|key| !key.expose_secret().is_empty());
    let credential_profile = credential_environment_entry(&request.credential, PROFILE_ENV);
    let scoped_profile = credential_profile
        .as_ref()
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| {
            request
                .environment
                .get(PROFILE_ENV)
                .filter(|value| !value.is_empty())
                .cloned()
        });
    let auth_ambient_profile = if credential_profile.is_some() {
        None
    } else {
        ambient_value_local(&request, PROFILE_ENV, cancellation.clone()).await?
    };
    let auth_profile = scoped_profile
        .clone()
        .or_else(|| auth_ambient_profile.clone());
    let client_ambient_profile =
        ambient_value_local(&request, PROFILE_ENV, cancellation.clone()).await?;
    let environment_bearer = auth_value_local(&request, BEARER_ENV, cancellation.clone()).await?;
    let auth_credentials = static_credentials_local(&request, false, cancellation.clone()).await?;
    let chain_source = default_chain_source_local(&request, cancellation.clone()).await?;
    let source = if credential_key.is_some() {
        Some("stored credential")
    } else if environment_bearer.is_some() {
        Some(BEARER_ENV)
    } else if credential_environment(&request.credential, PROFILE_ENV).is_some() {
        Some("stored credential")
    } else if auth_profile.is_some() {
        Some(PROFILE_ENV)
    } else if auth_credentials.is_some() {
        Some("AWS access keys")
    } else if chain_source.is_some() {
        chain_source
    } else {
        None
    };
    let Some(source) = source else {
        return Ok(None);
    };

    let retain_credential_environment = credential_key.is_some()
        || (environment_bearer.is_none() && auth_profile.is_some() && request.credential.is_some());
    let client_scoped_profile =
        configuration_scoped_value_local(&request, PROFILE_ENV, retain_credential_environment);
    let client_profile = client_scoped_profile
        .clone()
        .or_else(|| client_ambient_profile.clone());
    let configured_region = configuration_value_local(
        &request,
        "AWS_REGION",
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .or(configuration_value_local(
        &request,
        "AWS_DEFAULT_REGION",
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?);
    let skip_auth = configuration_value_local(
        &request,
        SKIP_AUTH_ENV,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .as_deref()
        == Some("1");
    let force_http1 = configuration_value_local(
        &request,
        FORCE_HTTP1_ENV,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .as_deref()
        == Some("1");
    let long_cache_retention = configuration_value_local(
        &request,
        CACHE_RETENTION_ENV,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .as_deref()
        == Some("long");
    let force_prompt_caching = configuration_value_local(
        &request,
        FORCE_CACHE_ENV,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?
    .as_deref()
        == Some("1");
    let proxy_environment = proxy_environment_local(
        &request,
        retain_credential_environment,
        cancellation.clone(),
    )
    .await?;
    let bearer_token = (!skip_auth)
        .then(|| {
            credential_key
                .clone()
                .or_else(|| environment_bearer.clone().map(SecretString::new))
        })
        .flatten();
    let credentials = if skip_auth {
        Some(dummy_credentials())
    } else if client_scoped_profile.is_none() {
        static_credentials_local(&request, retain_credential_environment, cancellation).await?
    } else {
        None
    };
    resolved_auth(
        source,
        BedrockSigningConfig {
            profile: client_profile,
            region: configured_region,
            endpoint: None,
            credentials,
            bearer_token,
            skip_auth,
            force_http1,
            proxy_url: None,
            long_cache_retention,
            force_prompt_caching,
            has_scoped_profile: client_scoped_profile.is_some(),
            has_ambient_profile: client_ambient_profile.is_some(),
            proxy_environment,
        },
    )
    .map(Some)
}

fn resolved_auth(source: &str, config: BedrockSigningConfig) -> Result<ResolvedAuth, AuthError> {
    let mut headers = HeaderMap::new();
    if let Some(token) = &config.bearer_token {
        let value =
            HeaderValue::from_str(&format!("Bearer {}", token.expose_secret())).map_err(|_| {
                AuthError::new(
                    "invalid_bearer_token",
                    "Bedrock bearer token cannot be encoded as an Authorization header",
                )
            })?;
        headers.insert(header::AUTHORIZATION, value);
    }
    let mut resolved = ResolvedAuth {
        api_key: None,
        headers,
        transport_headers: HeaderMap::new(),
        base_url: config.endpoint.clone(),
        source: AuthSource::new(source),
    };
    attach_signing_config(&mut resolved, &config)?;
    Ok(resolved)
}

fn credential_environment(credential: &Option<ApiKeyCredential>, name: &str) -> Option<String> {
    credential_environment_entry(credential, name).filter(|value| !value.is_empty())
}

fn credential_environment_entry(
    credential: &Option<ApiKeyCredential>,
    name: &str,
) -> Option<String> {
    credential
        .as_ref()
        .and_then(|credential| credential.environment.get(name))
        .cloned()
}

fn request_environment_value(request: &ApiKeyResolveRequest, name: &str) -> Option<String> {
    request
        .environment
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
}

fn local_request_environment_value(
    request: &LocalApiKeyResolveRequest,
    name: &str,
) -> Option<String> {
    request
        .environment
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
}

fn configuration_scoped_value_send(
    request: &ApiKeyResolveRequest,
    name: &str,
    include_credential_environment: bool,
) -> Option<String> {
    request_environment_value(request, name).or_else(|| {
        include_credential_environment
            .then(|| credential_environment(&request.credential, name))
            .flatten()
    })
}

fn configuration_scoped_value_local(
    request: &LocalApiKeyResolveRequest,
    name: &str,
    include_credential_environment: bool,
) -> Option<String> {
    local_request_environment_value(request, name).or_else(|| {
        include_credential_environment
            .then(|| credential_environment(&request.credential, name))
            .flatten()
    })
}

async fn ambient_value_send(
    request: &ApiKeyResolveRequest,
    name: &str,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    request
        .context
        .env(name.to_owned(), cancellation)
        .await
        .map(|value| value.filter(|value| !value.is_empty()))
}

async fn ambient_value_local(
    request: &LocalApiKeyResolveRequest,
    name: &str,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    request
        .context
        .env(name.to_owned(), cancellation)
        .await
        .map(|value| value.filter(|value| !value.is_empty()))
}

async fn auth_value_send(
    request: &ApiKeyResolveRequest,
    name: &str,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = request_environment_value(request, name) {
        return Ok(Some(value));
    }
    ambient_value_send(request, name, cancellation).await
}

async fn auth_value_local(
    request: &LocalApiKeyResolveRequest,
    name: &str,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = local_request_environment_value(request, name) {
        return Ok(Some(value));
    }
    ambient_value_local(request, name, cancellation).await
}

async fn configuration_value_send(
    request: &ApiKeyResolveRequest,
    name: &str,
    include_credential_environment: bool,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) =
        configuration_scoped_value_send(request, name, include_credential_environment)
    {
        return Ok(Some(value));
    }
    ambient_value_send(request, name, cancellation).await
}

async fn configuration_value_local(
    request: &LocalApiKeyResolveRequest,
    name: &str,
    include_credential_environment: bool,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) =
        configuration_scoped_value_local(request, name, include_credential_environment)
    {
        return Ok(Some(value));
    }
    ambient_value_local(request, name, cancellation).await
}

async fn proxy_environment_send(
    request: &ApiKeyResolveRequest,
    include_credential_environment: bool,
    cancellation: CancellationToken,
) -> Result<BedrockProxyEnvironment, AuthError> {
    Ok(BedrockProxyEnvironment {
        http_proxy: proxy_value_send(
            request,
            HTTP_PROXY_ENV,
            include_credential_environment,
            cancellation.clone(),
        )
        .await?,
        https_proxy: proxy_value_send(
            request,
            HTTPS_PROXY_ENV,
            include_credential_environment,
            cancellation.clone(),
        )
        .await?,
        all_proxy: proxy_value_send(
            request,
            ALL_PROXY_ENV,
            include_credential_environment,
            cancellation.clone(),
        )
        .await?,
        no_proxy: proxy_value_send(
            request,
            NO_PROXY_ENV,
            include_credential_environment,
            cancellation,
        )
        .await?,
    })
}

async fn proxy_environment_local(
    request: &LocalApiKeyResolveRequest,
    include_credential_environment: bool,
    cancellation: CancellationToken,
) -> Result<BedrockProxyEnvironment, AuthError> {
    Ok(BedrockProxyEnvironment {
        http_proxy: proxy_value_local(
            request,
            HTTP_PROXY_ENV,
            include_credential_environment,
            cancellation.clone(),
        )
        .await?,
        https_proxy: proxy_value_local(
            request,
            HTTPS_PROXY_ENV,
            include_credential_environment,
            cancellation.clone(),
        )
        .await?,
        all_proxy: proxy_value_local(
            request,
            ALL_PROXY_ENV,
            include_credential_environment,
            cancellation.clone(),
        )
        .await?,
        no_proxy: proxy_value_local(
            request,
            NO_PROXY_ENV,
            include_credential_environment,
            cancellation,
        )
        .await?,
    })
}

async fn proxy_value_send(
    request: &ApiKeyResolveRequest,
    lowercase_name: &str,
    include_credential_environment: bool,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    let uppercase_name = lowercase_name.to_ascii_uppercase();
    if let Some(value) =
        configuration_scoped_value_send(request, lowercase_name, include_credential_environment)
    {
        return Ok(Some(value));
    }
    if let Some(value) =
        configuration_scoped_value_send(request, &uppercase_name, include_credential_environment)
    {
        return Ok(Some(value));
    }
    if let Some(value) = ambient_value_send(request, lowercase_name, cancellation.clone()).await? {
        return Ok(Some(value));
    }
    ambient_value_send(request, &uppercase_name, cancellation).await
}

async fn proxy_value_local(
    request: &LocalApiKeyResolveRequest,
    lowercase_name: &str,
    include_credential_environment: bool,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    let uppercase_name = lowercase_name.to_ascii_uppercase();
    if let Some(value) =
        configuration_scoped_value_local(request, lowercase_name, include_credential_environment)
    {
        return Ok(Some(value));
    }
    if let Some(value) =
        configuration_scoped_value_local(request, &uppercase_name, include_credential_environment)
    {
        return Ok(Some(value));
    }
    if let Some(value) = ambient_value_local(request, lowercase_name, cancellation.clone()).await? {
        return Ok(Some(value));
    }
    ambient_value_local(request, &uppercase_name, cancellation).await
}

async fn static_credentials_send(
    request: &ApiKeyResolveRequest,
    include_credential_environment: bool,
    cancellation: CancellationToken,
) -> Result<Option<BedrockStaticCredentials>, AuthError> {
    let access_key_id = configuration_value_send(
        request,
        "AWS_ACCESS_KEY_ID",
        include_credential_environment,
        cancellation.clone(),
    )
    .await?;
    let secret_access_key = configuration_value_send(
        request,
        "AWS_SECRET_ACCESS_KEY",
        include_credential_environment,
        cancellation.clone(),
    )
    .await?;
    let (Some(access_key_id), Some(secret_access_key)) = (access_key_id, secret_access_key) else {
        return Ok(None);
    };
    let session_token = configuration_value_send(
        request,
        "AWS_SESSION_TOKEN",
        include_credential_environment,
        cancellation,
    )
    .await?;
    Ok(Some(BedrockStaticCredentials {
        access_key_id: SecretString::new(access_key_id),
        secret_access_key: SecretString::new(secret_access_key),
        session_token: session_token.map(SecretString::new),
    }))
}

async fn static_credentials_local(
    request: &LocalApiKeyResolveRequest,
    include_credential_environment: bool,
    cancellation: CancellationToken,
) -> Result<Option<BedrockStaticCredentials>, AuthError> {
    let access_key_id = configuration_value_local(
        request,
        "AWS_ACCESS_KEY_ID",
        include_credential_environment,
        cancellation.clone(),
    )
    .await?;
    let secret_access_key = configuration_value_local(
        request,
        "AWS_SECRET_ACCESS_KEY",
        include_credential_environment,
        cancellation.clone(),
    )
    .await?;
    let (Some(access_key_id), Some(secret_access_key)) = (access_key_id, secret_access_key) else {
        return Ok(None);
    };
    let session_token = configuration_value_local(
        request,
        "AWS_SESSION_TOKEN",
        include_credential_environment,
        cancellation,
    )
    .await?;
    Ok(Some(BedrockStaticCredentials {
        access_key_id: SecretString::new(access_key_id),
        secret_access_key: SecretString::new(secret_access_key),
        session_token: session_token.map(SecretString::new),
    }))
}

async fn default_chain_source_send(
    request: &ApiKeyResolveRequest,
    cancellation: CancellationToken,
) -> Result<Option<&'static str>, AuthError> {
    for (name, source) in [
        ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "ECS task role"),
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", "ECS task role"),
        ("AWS_WEB_IDENTITY_TOKEN_FILE", "web identity token"),
    ] {
        if auth_value_send(request, name, cancellation.clone())
            .await?
            .is_some()
        {
            return Ok(Some(source));
        }
    }
    Ok(None)
}

async fn default_chain_source_local(
    request: &LocalApiKeyResolveRequest,
    cancellation: CancellationToken,
) -> Result<Option<&'static str>, AuthError> {
    for (name, source) in [
        ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "ECS task role"),
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", "ECS task role"),
        ("AWS_WEB_IDENTITY_TOKEN_FILE", "web identity token"),
    ] {
        if auth_value_local(request, name, cancellation.clone())
            .await?
            .is_some()
        {
            return Ok(Some(source));
        }
    }
    Ok(None)
}

fn dummy_credentials() -> BedrockStaticCredentials {
    BedrockStaticCredentials {
        access_key_id: SecretString::new("dummy-access-key"),
        secret_access_key: SecretString::new("dummy-secret-key"),
        session_token: None,
    }
}

fn model_arn_region(model_id: Option<&str>) -> Option<&str> {
    let value = model_id?;
    let mut fields = value.split(':');
    let arn = fields.next()?;
    let partition = fields.next()?;
    let service = fields.next()?;
    let region = fields.next()?;
    let valid_partition = partition == "aws"
        || partition
            .strip_prefix("aws-")
            .is_some_and(|suffix| !suffix.is_empty());
    (arn == "arn"
        && valid_partition
        && partition.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && service == "bedrock"
        && !region.is_empty()
        && region.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        }))
    .then_some(region)
}

fn standard_endpoint_region(url: &Url) -> Option<&str> {
    let host = url.host_str()?;
    let rest = host
        .strip_prefix("bedrock-runtime.")
        .or_else(|| host.strip_prefix("bedrock-runtime-fips."))?;
    let region = rest
        .strip_suffix(".amazonaws.com")
        .or_else(|| rest.strip_suffix(".amazonaws.com.cn"))?;
    (!region.is_empty()
        && region.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        }))
    .then_some(region)
}

fn finalize_signing_config(
    config: &mut BedrockSigningConfig,
    model: Option<(&str, &Url)>,
) -> Result<(), AuthError> {
    let Some((model_id, model_url)) = model else {
        return Ok(());
    };
    let endpoint_region = standard_endpoint_region(model_url);
    let use_explicit_endpoint =
        endpoint_region.is_none() || (config.region.is_none() && !config.has_ambient_profile);
    config.region = model_arn_region(Some(model_id))
        .map(str::to_owned)
        .or_else(|| config.region.clone())
        .or_else(|| {
            use_explicit_endpoint
                .then(|| endpoint_region.map(str::to_owned))
                .flatten()
        })
        .or_else(|| (!config.has_ambient_profile).then(|| "us-east-1".to_owned()));
    config.endpoint = use_explicit_endpoint.then(|| model_url.clone());
    config.proxy_url = resolve_proxy_url_for_target(model_url, &config.proxy_environment)?;
    config.force_http1 |= config.proxy_url.is_some();
    Ok(())
}

fn resolve_proxy_url_for_target(
    target: &Url,
    environment: &BedrockProxyEnvironment,
) -> Result<Option<Url>, AuthError> {
    if !should_proxy_target(target, environment.no_proxy.as_deref()) {
        return Ok(None);
    }
    let proxy = match target.scheme() {
        "http" => environment.http_proxy.as_deref(),
        "https" => environment.https_proxy.as_deref(),
        _ => None,
    }
    .or(environment.all_proxy.as_deref());
    let Some(proxy) = proxy else {
        return Ok(None);
    };
    let proxy = if proxy.contains("://") {
        proxy.to_owned()
    } else {
        format!("{}://{proxy}", target.scheme())
    };
    let proxy_url = Url::parse(&proxy).map_err(|error| {
        AuthError::new(
            "invalid_proxy_url",
            format!("Invalid proxy URL {proxy:?}: {error}"),
        )
    })?;
    if !matches!(proxy_url.scheme(), "http" | "https") {
        return Err(AuthError::new(
            "unsupported_proxy_protocol",
            format!(
                "{UNSUPPORTED_PROXY_PROTOCOL_MESSAGE} Got {}:",
                proxy_url.scheme()
            ),
        ));
    }
    Ok(Some(proxy_url))
}

fn should_proxy_target(target: &Url, no_proxy: Option<&str>) -> bool {
    let Some(no_proxy) = no_proxy else {
        return true;
    };
    let no_proxy = no_proxy.to_lowercase();
    if no_proxy.is_empty() {
        return true;
    }
    if no_proxy == "*" {
        return false;
    }
    let Some(hostname) = target.host_str() else {
        return true;
    };
    let port = target.port_or_known_default().unwrap_or(0);
    no_proxy
        .split(|character: char| character == ',' || is_ecmascript_whitespace(character))
        .filter(|entry| !entry.is_empty())
        .all(|entry| {
            let (mut proxy_hostname, proxy_port) = entry
                .rsplit_once(':')
                .filter(|(_, port)| {
                    !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
                })
                .map_or((entry, 0_u64), |(host, port)| {
                    // Pi uses Number.parseInt rather than a network-port type.
                    // Oversized numeric entries remain truthy nonmatching
                    // ports; an integer overflow must not turn them into zero
                    // and accidentally bypass the proxy.
                    (host, port.parse::<u64>().unwrap_or(u64::MAX))
                });
            if proxy_port != 0 && proxy_port != u64::from(port) {
                return true;
            }
            if !proxy_hostname.starts_with(['.', '*']) {
                return hostname != proxy_hostname;
            }
            if let Some(without_star) = proxy_hostname.strip_prefix('*') {
                proxy_hostname = without_star;
            }
            !hostname.ends_with(proxy_hostname)
        })
}

fn authentication_method_prompt() -> AuthPrompt {
    AuthPrompt::Select {
        message: "Select Amazon Bedrock authentication method:".to_owned(),
        options: vec![
            AuthSelectOption {
                id: "bearer-token".to_owned(),
                label: "Bearer token".to_owned(),
                description: None,
            },
            AuthSelectOption {
                id: "aws-profile".to_owned(),
                label: "AWS profile".to_owned(),
                description: None,
            },
            AuthSelectOption {
                id: "credential-chain".to_owned(),
                label: "Existing AWS credential chain".to_owned(),
                description: None,
            },
        ],
    }
}

fn notify_credential_chain(interaction: &dyn AuthInteraction) -> Result<(), AuthError> {
    interaction
        .notify(credential_chain_event())
        .map_err(AuthError::from)
}

fn notify_local_credential_chain(interaction: &dyn LocalAuthInteraction) -> Result<(), AuthError> {
    interaction
        .notify(credential_chain_event())
        .map_err(AuthError::from)
}

fn credential_chain_event() -> AuthEvent {
    AuthEvent::Info {
        message:
            "Amazon Bedrock supports AWS profiles, IAM credentials, and role-based credentials."
                .to_owned(),
        links: vec![AuthInfoLink {
            label: Some("AWS credential provider chain".to_owned()),
            url: Url::parse(
                "https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html",
            )
            .expect("static AWS credential-chain documentation URL is valid"),
        }],
    }
}

async fn login_send(
    interaction: Arc<dyn AuthInteraction>,
    method: String,
    cancellation: CancellationToken,
) -> Result<ApiKeyCredential, AuthError> {
    match method.as_str() {
        "bearer-token" => {
            let answer = interaction
                .prompt(
                    AuthPrompt::Secret {
                        message: "Enter Amazon Bedrock bearer token".to_owned(),
                        placeholder: None,
                    },
                    cancellation,
                )
                .await?;
            let AuthAnswer::Text(key) = answer else {
                return Err(AuthError::new(
                    "invalid_auth_answer",
                    "Bedrock bearer-token prompt returned a non-text answer",
                ));
            };
            Ok(ApiKeyCredential {
                key: Some(SecretString::new(key)),
                environment: Default::default(),
            })
        }
        "aws-profile" => {
            notify_credential_chain(interaction.as_ref())?;
            let answer = interaction
                .prompt(
                    AuthPrompt::Text {
                        message: "Enter AWS profile name".to_owned(),
                        placeholder: None,
                    },
                    cancellation,
                )
                .await?;
            let AuthAnswer::Text(profile) = answer else {
                return Err(AuthError::new(
                    "invalid_auth_answer",
                    "Bedrock profile prompt returned a non-text answer",
                ));
            };
            Ok(ApiKeyCredential {
                key: None,
                environment: [(PROFILE_ENV.to_owned(), profile)].into_iter().collect(),
            })
        }
        "credential-chain" => {
            notify_credential_chain(interaction.as_ref())?;
            let _ = interaction
                .prompt(
                    AuthPrompt::Text {
                        message: "Configure AWS credentials, then press Enter to continue"
                            .to_owned(),
                        placeholder: None,
                    },
                    cancellation,
                )
                .await?;
            Ok(ApiKeyCredential {
                key: None,
                environment: Default::default(),
            })
        }
        other => Err(AuthError::new(
            "unknown_auth_method",
            format!("unknown Amazon Bedrock auth method: {other}"),
        )),
    }
}

async fn login_local(
    interaction: Rc<dyn LocalAuthInteraction>,
    method: String,
    cancellation: CancellationToken,
) -> Result<ApiKeyCredential, AuthError> {
    match method.as_str() {
        "bearer-token" => {
            let answer = interaction
                .prompt(
                    AuthPrompt::Secret {
                        message: "Enter Amazon Bedrock bearer token".to_owned(),
                        placeholder: None,
                    },
                    cancellation,
                )
                .await?;
            let AuthAnswer::Text(key) = answer else {
                return Err(AuthError::new(
                    "invalid_auth_answer",
                    "Bedrock bearer-token prompt returned a non-text answer",
                ));
            };
            Ok(ApiKeyCredential {
                key: Some(SecretString::new(key)),
                environment: Default::default(),
            })
        }
        "aws-profile" => {
            notify_local_credential_chain(interaction.as_ref())?;
            let answer = interaction
                .prompt(
                    AuthPrompt::Text {
                        message: "Enter AWS profile name".to_owned(),
                        placeholder: None,
                    },
                    cancellation,
                )
                .await?;
            let AuthAnswer::Text(profile) = answer else {
                return Err(AuthError::new(
                    "invalid_auth_answer",
                    "Bedrock profile prompt returned a non-text answer",
                ));
            };
            Ok(ApiKeyCredential {
                key: None,
                environment: [(PROFILE_ENV.to_owned(), profile)].into_iter().collect(),
            })
        }
        "credential-chain" => {
            notify_local_credential_chain(interaction.as_ref())?;
            let _ = interaction
                .prompt(
                    AuthPrompt::Text {
                        message: "Configure AWS credentials, then press Enter to continue"
                            .to_owned(),
                        placeholder: None,
                    },
                    cancellation,
                )
                .await?;
            Ok(ApiKeyCredential {
                key: None,
                environment: Default::default(),
            })
        }
        other => Err(AuthError::new(
            "unknown_auth_method",
            format!("unknown Amazon Bedrock auth method: {other}"),
        )),
    }
}
