//! Cloudflare Workers AI provider leaf crate.

#![deny(missing_docs)]

use agentprism_ai::{
    ApiId, ApiKeyAuth, ApiKeyCredential, ApiKeyResolveRequest, AuthAnswer, AuthError,
    AuthInteraction, AuthPrompt, AuthResolver, AuthSource, CancellationToken, LocalApiKeyAuth,
    LocalApiKeyResolveRequest, LocalAuthInteraction, LocalAuthResolver, LocalBoxFuture,
    LocalProviderAuthResolver, LocalResolveAuthRequest, ProviderAuthResolver, ResolveAuthRequest,
    ResolvedAuth, SecretString, SendBoxFuture,
};
use http::{HeaderMap, HeaderValue, header};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

const ACCOUNT_HEADER: &str = "x-pi-cloudflare-account-id";

/// Returns the pinned Cloudflare Workers AI catalog owned by this leaf.
pub fn models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    agentprism_openai::parse_openai_published_catalog(
        include_str!("../data/models.json"),
        "cloudflare-workers-ai",
        "openai-completions",
    )
    .map_err(ProviderBuildError::catalog)
}

/// Builds the Send Cloudflare Workers AI registration.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    agentprism_provider_common::build_provider(
        "cloudflare-workers-ai",
        "Cloudflare Workers AI",
        models()?,
        Arc::new(WorkersResolver {
            inner: ProviderAuthResolver::new(Some(Arc::new(WorkersKeyAuth)), None),
        }),
        [(
            ApiId::new("openai-completions"),
            agentprism_openai::openai_completions_api(inputs.http)
                as Arc<dyn agentprism_ai::ChatApi>,
        )],
    )
}

/// Builds the local Cloudflare Workers AI registration.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    agentprism_provider_common::build_local_provider(
        "cloudflare-workers-ai",
        "Cloudflare Workers AI",
        models()?,
        Rc::new(LocalWorkersResolver {
            inner: LocalProviderAuthResolver::new(Some(Rc::new(WorkersKeyAuth)), None),
        }),
        [(
            ApiId::new("openai-completions"),
            agentprism_openai::local_openai_completions_api(inputs.http)
                as Rc<dyn agentprism_ai::LocalChatApi>,
        )],
    )
}

struct WorkersResolver {
    inner: ProviderAuthResolver,
}

impl AuthResolver for WorkersResolver {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let model_url = request
                .model
                .as_ref()
                .map(|model| model.common.base_url.clone());
            let Some(mut auth) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            finish_auth(model_url.as_ref(), &mut auth)?;
            Ok(Some(auth))
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

struct LocalWorkersResolver {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalWorkersResolver {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let model_url = request
                .model
                .as_ref()
                .map(|model| model.common.base_url.clone());
            let Some(mut auth) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            finish_auth(model_url.as_ref(), &mut auth)?;
            Ok(Some(auth))
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

#[derive(Clone, Copy)]
struct WorkersKeyAuth;

impl ApiKeyAuth for WorkersKeyAuth {
    fn name(&self) -> &str {
        "Cloudflare API key"
    }

    fn resolve(
        &self,
        request: ApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let key = value_send("CLOUDFLARE_API_KEY", &request, cancellation.clone()).await?;
            let account = value_send("CLOUDFLARE_ACCOUNT_ID", &request, cancellation).await?;
            resolved(key, account, request.credential.is_some())
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        Box::pin(async move {
            let key = prompt_send(
                interaction.as_ref(),
                true,
                "Enter Cloudflare API key",
                cancellation.clone(),
            )
            .await?;
            let account = prompt_send(
                interaction.as_ref(),
                false,
                "Enter Cloudflare account ID",
                cancellation,
            )
            .await?;
            Ok(credential(key, account))
        })
    }
}

impl LocalApiKeyAuth for WorkersKeyAuth {
    fn name(&self) -> &str {
        "Cloudflare API key"
    }

    fn resolve(
        &self,
        request: LocalApiKeyResolveRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let key = value_local("CLOUDFLARE_API_KEY", &request, cancellation.clone()).await?;
            let account = value_local("CLOUDFLARE_ACCOUNT_ID", &request, cancellation).await?;
            resolved(key, account, request.credential.is_some())
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ApiKeyCredential, AuthError>> {
        Box::pin(async move {
            let key = prompt_local(
                interaction.as_ref(),
                true,
                "Enter Cloudflare API key",
                cancellation.clone(),
            )
            .await?;
            let account = prompt_local(
                interaction.as_ref(),
                false,
                "Enter Cloudflare account ID",
                cancellation,
            )
            .await?;
            Ok(credential(key, account))
        })
    }
}

fn resolved(
    key: Option<String>,
    account: Option<String>,
    stored: bool,
) -> Result<Option<ResolvedAuth>, AuthError> {
    let (Some(key), Some(account)) = (nonempty(key), nonempty(account)) else {
        return Ok(None);
    };
    let mut transport_headers = HeaderMap::new();
    transport_headers.insert(
        ACCOUNT_HEADER,
        HeaderValue::from_str(&account)
            .map_err(|_| AuthError::new("cloudflare_auth", "invalid Cloudflare account ID"))?,
    );
    Ok(Some(ResolvedAuth {
        api_key: Some(SecretString::new(key)),
        headers: HeaderMap::new(),
        transport_headers,
        base_url: None,
        source: AuthSource::new(if stored {
            "stored credential"
        } else {
            "CLOUDFLARE_API_KEY"
        }),
    }))
}

fn finish_auth(model_url: Option<&url::Url>, auth: &mut ResolvedAuth) -> Result<(), AuthError> {
    let key = auth
        .api_key
        .take()
        .ok_or_else(|| AuthError::new("cloudflare_auth", "Cloudflare API key was not resolved"))?;
    auth.headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key.expose_secret()))
            .map_err(|_| AuthError::new("cloudflare_auth", "invalid Cloudflare API key"))?,
    );
    let account = auth
        .transport_headers
        .remove(ACCOUNT_HEADER)
        .and_then(|value| value.to_str().ok().map(str::to_owned));
    if let (Some(model_url), Some(account)) = (model_url, account) {
        let value = model_url
            .as_str()
            .replace("%7BCLOUDFLARE_ACCOUNT_ID%7D", &account)
            .replace("%7bCLOUDFLARE_ACCOUNT_ID%7d", &account);
        auth.base_url = Some(url::Url::parse(&value).map_err(|_| {
            AuthError::new(
                "cloudflare_endpoint",
                "invalid resolved Cloudflare endpoint",
            )
        })?);
    }
    Ok(())
}

async fn value_send(
    name: &str,
    request: &ApiKeyResolveRequest,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = credential_value(name, request.credential.as_ref()) {
        return Ok(Some(value));
    }
    if let Some(value) = request.environment.get(name) {
        return Ok(Some(value.clone()));
    }
    request.context.env(name.into(), cancellation).await
}

async fn value_local(
    name: &str,
    request: &LocalApiKeyResolveRequest,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    if let Some(value) = credential_value(name, request.credential.as_ref()) {
        return Ok(Some(value));
    }
    if let Some(value) = request.environment.get(name) {
        return Ok(Some(value.clone()));
    }
    request.context.env(name.into(), cancellation).await
}

fn credential_value(name: &str, credential: Option<&ApiKeyCredential>) -> Option<String> {
    let credential = credential?;
    if name == "CLOUDFLARE_API_KEY" {
        credential
            .key
            .as_ref()
            .map(|key| key.expose_secret().to_owned())
    } else {
        credential.environment.get(name).cloned()
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn credential(key: String, account: String) -> ApiKeyCredential {
    ApiKeyCredential {
        key: Some(SecretString::new(key)),
        environment: BTreeMap::from([("CLOUDFLARE_ACCOUNT_ID".into(), account)]),
    }
}

async fn prompt_send(
    interaction: &dyn AuthInteraction,
    secret: bool,
    message: &str,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    required_answer(
        interaction
            .prompt(prompt(secret, message), cancellation)
            .await?,
    )
}

async fn prompt_local(
    interaction: &dyn LocalAuthInteraction,
    secret: bool,
    message: &str,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    required_answer(
        interaction
            .prompt(prompt(secret, message), cancellation)
            .await?,
    )
}

fn prompt(secret: bool, message: &str) -> AuthPrompt {
    if secret {
        AuthPrompt::Secret {
            message: message.into(),
            placeholder: None,
        }
    } else {
        AuthPrompt::Text {
            message: message.into(),
            placeholder: None,
        }
    }
}

fn required_answer(answer: AuthAnswer) -> Result<String, AuthError> {
    let AuthAnswer::Text(value) = answer else {
        return Err(AuthError::new("cloudflare_auth", "expected a text answer"));
    };
    if value.is_empty() {
        return Err(AuthError::new(
            "cloudflare_auth",
            "Cloudflare credential fields must not be empty",
        ));
    }
    Ok(value)
}
