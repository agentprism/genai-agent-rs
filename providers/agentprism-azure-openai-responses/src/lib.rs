//! Azure OpenAI Responses provider leaf crate.

#![deny(missing_docs)]

use agentprism_ai::{
    ApiId, ApiKeyAuth, ApiKeyResolveRequest, AuthError, AuthInteraction, AuthResolver, AuthSource,
    CancellationToken, EnvironmentApiKeyAuth, LocalApiKeyAuth, LocalApiKeyResolveRequest,
    LocalAuthInteraction, LocalAuthResolver, LocalBoxFuture, LocalProviderAuthResolver,
    LocalResolveAuthRequest, ProviderAuthResolver, ResolveAuthRequest, ResolvedAuth, SendBoxFuture,
    trim_ecmascript,
};
use http::{HeaderMap, HeaderValue};
use std::rc::Rc;
use std::sync::Arc;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

const SOURCE: &str = include_str!("../data/models.json");

/// Returns the pinned Azure OpenAI Responses catalog owned by this leaf.
pub fn models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    agentprism_openai::parse_openai_published_catalog(
        SOURCE,
        "azure-openai-responses",
        "azure-openai-responses",
    )
    .map_err(ProviderBuildError::catalog)
}

/// Builds the Send Azure OpenAI Responses registration.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    agentprism_provider_common::build_provider(
        "azure-openai-responses",
        "Azure OpenAI",
        models()?,
        Arc::new(AzureResolver {
            inner: ProviderAuthResolver::new(Some(Arc::new(AzureApiKeyAuth)), None),
        }),
        [(
            ApiId::new("azure-openai-responses"),
            agentprism_openai::azure_openai_responses_api(inputs.http)
                as Arc<dyn agentprism_ai::ChatApi>,
        )],
    )
}

/// Builds the local Azure OpenAI Responses registration.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    agentprism_provider_common::build_local_provider(
        "azure-openai-responses",
        "Azure OpenAI",
        models()?,
        Rc::new(LocalAzureResolver {
            inner: LocalProviderAuthResolver::new(Some(Rc::new(AzureApiKeyAuth)), None),
        }),
        [(
            ApiId::new("azure-openai-responses"),
            agentprism_openai::local_azure_openai_responses_api(inputs.http)
                as Rc<dyn agentprism_ai::LocalChatApi>,
        )],
    )
}

struct AzureResolver {
    inner: ProviderAuthResolver,
}

impl AuthResolver for AzureResolver {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let requires_endpoint = request
                .model
                .as_ref()
                .is_some_and(|model| model.common.base_url.host_str() == Some("azure.invalid"));
            let resolved = self.inner.resolve(request, cancellation).await?;
            if requires_endpoint
                && resolved
                    .as_ref()
                    .and_then(|auth| auth.base_url.as_ref())
                    .is_none()
            {
                return Err(AuthError::new(
                    "azure_openai_endpoint",
                    "Azure OpenAI requires azureBaseUrl/AZURE_OPENAI_BASE_URL or azureResourceName/AZURE_OPENAI_RESOURCE_NAME",
                ));
            }
            Ok(resolved)
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<agentprism_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

struct LocalAzureResolver {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalAzureResolver {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let requires_endpoint = request
                .model
                .as_ref()
                .is_some_and(|model| model.common.base_url.host_str() == Some("azure.invalid"));
            let resolved = self.inner.resolve(request, cancellation).await?;
            if requires_endpoint
                && resolved
                    .as_ref()
                    .and_then(|auth| auth.base_url.as_ref())
                    .is_none()
            {
                return Err(AuthError::new(
                    "azure_openai_endpoint",
                    "Azure OpenAI requires azureBaseUrl/AZURE_OPENAI_BASE_URL or azureResourceName/AZURE_OPENAI_RESOURCE_NAME",
                ));
            }
            Ok(resolved)
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<agentprism_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

#[derive(Clone, Copy, Debug)]
struct AzureApiKeyAuth;

impl ApiKeyAuth for AzureApiKeyAuth {
    fn name(&self) -> &str {
        "Azure OpenAI API key"
    }

    fn resolve(
        &self,
        request: ApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let stored_environment = request
                .credential
                .as_ref()
                .map(|credential| credential.environment.clone())
                .unwrap_or_default();
            let explicit_environment = request.environment.clone();
            let context = Arc::clone(&request.context);
            let method =
                EnvironmentApiKeyAuth::new("Azure OpenAI API key", ["AZURE_OPENAI_API_KEY"]);
            let Some(mut resolved) =
                ApiKeyAuth::resolve(&method, request, cancellation.clone()).await?
            else {
                return Ok(None);
            };
            augment_azure_auth_send(
                &mut resolved,
                &explicit_environment,
                &stored_environment,
                context.as_ref(),
                cancellation,
            )
            .await?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<agentprism_ai::ApiKeyCredential, AuthError>> {
        Box::pin(async move { prompt_key_send(interaction.as_ref(), cancellation).await })
    }
}

impl LocalApiKeyAuth for AzureApiKeyAuth {
    fn name(&self) -> &str {
        "Azure OpenAI API key"
    }

    fn resolve(
        &self,
        request: LocalApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let stored_environment = request
                .credential
                .as_ref()
                .map(|credential| credential.environment.clone())
                .unwrap_or_default();
            let explicit_environment = request.environment.clone();
            let context = Rc::clone(&request.context);
            let Some(mut resolved) = LocalApiKeyAuth::resolve(
                &EnvironmentApiKeyAuth::new("Azure OpenAI API key", ["AZURE_OPENAI_API_KEY"]),
                request,
                cancellation.clone(),
            )
            .await?
            else {
                return Ok(None);
            };
            augment_azure_auth_local(
                &mut resolved,
                &explicit_environment,
                &stored_environment,
                context.as_ref(),
                cancellation,
            )
            .await?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<agentprism_ai::ApiKeyCredential, AuthError>> {
        Box::pin(async move { prompt_key_local(interaction.as_ref(), cancellation).await })
    }
}

async fn prompt_key_send(
    interaction: &dyn AuthInteraction,
    cancellation: CancellationToken,
) -> Result<agentprism_ai::ApiKeyCredential, AuthError> {
    let answer = interaction
        .prompt(
            agentprism_ai::AuthPrompt::Secret {
                message: "Enter Azure OpenAI API key".into(),
                placeholder: None,
            },
            cancellation,
        )
        .await?;
    credential_from_answer(answer)
}

async fn prompt_key_local(
    interaction: &dyn LocalAuthInteraction,
    cancellation: CancellationToken,
) -> Result<agentprism_ai::ApiKeyCredential, AuthError> {
    let answer = interaction
        .prompt(
            agentprism_ai::AuthPrompt::Secret {
                message: "Enter Azure OpenAI API key".into(),
                placeholder: None,
            },
            cancellation,
        )
        .await?;
    credential_from_answer(answer)
}

fn credential_from_answer(
    answer: agentprism_ai::AuthAnswer,
) -> Result<agentprism_ai::ApiKeyCredential, AuthError> {
    let agentprism_ai::AuthAnswer::Text(key) = answer else {
        return Err(AuthError::new(
            "invalid_auth_answer",
            "secret prompt returned a non-text answer",
        ));
    };
    if key.trim().is_empty() {
        return Err(AuthError::new(
            "empty_api_key",
            "Azure OpenAI API key is required",
        ));
    }
    Ok(agentprism_ai::ApiKeyCredential {
        key: Some(agentprism_ai::SecretString::new(key)),
        environment: Default::default(),
    })
}

async fn augment_azure_auth_send(
    resolved: &mut ResolvedAuth,
    explicit: &std::collections::BTreeMap<String, String>,
    stored: &std::collections::BTreeMap<String, String>,
    context: &dyn agentprism_ai::AuthContext,
    cancellation: CancellationToken,
) -> Result<(), AuthError> {
    let base = value_send(
        "AZURE_OPENAI_BASE_URL",
        explicit,
        stored,
        context,
        cancellation.clone(),
    )
    .await?;
    let resource = value_send(
        "AZURE_OPENAI_RESOURCE_NAME",
        explicit,
        stored,
        context,
        cancellation.clone(),
    )
    .await?;
    let api_version = value_send(
        "AZURE_OPENAI_API_VERSION",
        explicit,
        stored,
        context,
        cancellation.clone(),
    )
    .await?;
    let deployment_map = value_send(
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
        explicit,
        stored,
        context,
        cancellation,
    )
    .await?;
    finish_azure_auth(resolved, base, resource, api_version, deployment_map)
}

async fn augment_azure_auth_local(
    resolved: &mut ResolvedAuth,
    explicit: &std::collections::BTreeMap<String, String>,
    stored: &std::collections::BTreeMap<String, String>,
    context: &dyn agentprism_ai::LocalAuthContext,
    cancellation: CancellationToken,
) -> Result<(), AuthError> {
    let base = value_local(
        "AZURE_OPENAI_BASE_URL",
        explicit,
        stored,
        context,
        cancellation.clone(),
    )
    .await?;
    let resource = value_local(
        "AZURE_OPENAI_RESOURCE_NAME",
        explicit,
        stored,
        context,
        cancellation.clone(),
    )
    .await?;
    let api_version = value_local(
        "AZURE_OPENAI_API_VERSION",
        explicit,
        stored,
        context,
        cancellation.clone(),
    )
    .await?;
    let deployment_map = value_local(
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
        explicit,
        stored,
        context,
        cancellation,
    )
    .await?;
    finish_azure_auth(resolved, base, resource, api_version, deployment_map)
}

fn finish_azure_auth(
    resolved: &mut ResolvedAuth,
    base: Option<String>,
    resource: Option<String>,
    api_version: Option<String>,
    deployment_map: Option<String>,
) -> Result<(), AuthError> {
    let key = resolved
        .api_key
        .take()
        .ok_or_else(|| AuthError::new("azure_openai_api_key", "Azure API key was not resolved"))?;
    resolved.headers.insert(
        "api-key",
        HeaderValue::from_str(key.expose_secret()).map_err(|_| {
            AuthError::new(
                "azure_openai_api_key",
                "Azure API key is not a valid header",
            )
        })?,
    );
    resolved.source = AuthSource::new("Azure OpenAI API key");
    resolved.base_url = azure_base_url(base.as_deref(), resource.as_deref())?;
    if let Some(value) = api_version {
        insert_private_header(
            &mut resolved.transport_headers,
            agentprism_openai::AZURE_API_VERSION_AUTH_HEADER,
            &value,
        )?;
    }
    if let Some(value) = deployment_map {
        insert_private_header(
            &mut resolved.transport_headers,
            agentprism_openai::AZURE_DEPLOYMENT_MAP_AUTH_HEADER,
            &value,
        )?;
    }
    Ok(())
}

fn azure_base_url(
    base: Option<&str>,
    resource: Option<&str>,
) -> Result<Option<url::Url>, AuthError> {
    let value = base
        .map(trim_ecmascript)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            resource
                .filter(|value| !value.is_empty())
                .map(|resource| format!("https://{resource}.openai.azure.com/openai/v1"))
        });
    value
        .map(|value| {
            agentprism_openai::normalize_azure_openai_base_url(&value).map_err(|error| {
                AuthError::new(
                    "invalid_azure_openai_base_url",
                    format!("Invalid Azure OpenAI base URL: {error}"),
                )
            })
        })
        .transpose()
}

fn insert_private_header(
    headers: &mut HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), AuthError> {
    headers.insert(
        http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AuthError::new("azure_auth", "invalid private Azure header name"))?,
        HeaderValue::from_str(value)
            .map_err(|_| AuthError::new("azure_auth", "invalid Azure environment value"))?,
    );
    Ok(())
}

async fn value_send(
    name: &str,
    explicit: &std::collections::BTreeMap<String, String>,
    stored: &std::collections::BTreeMap<String, String>,
    context: &dyn agentprism_ai::AuthContext,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = explicit.get(name).filter(|value| !value.is_empty()) {
        return Ok(Some(value.clone()));
    }
    if let Some(value) = stored.get(name).filter(|value| !value.is_empty()) {
        return Ok(Some(value.clone()));
    }
    context
        .env(name.to_owned(), cancellation)
        .await
        .map(|value| value.filter(|value| !value.is_empty()))
}

async fn value_local(
    name: &str,
    explicit: &std::collections::BTreeMap<String, String>,
    stored: &std::collections::BTreeMap<String, String>,
    context: &dyn agentprism_ai::LocalAuthContext,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = explicit.get(name).filter(|value| !value.is_empty()) {
        return Ok(Some(value.clone()));
    }
    if let Some(value) = stored.get(name).filter(|value| !value.is_empty()) {
        return Ok(Some(value.clone()));
    }
    context
        .env(name.to_owned(), cancellation)
        .await
        .map(|value| value.filter(|value| !value.is_empty()))
}
