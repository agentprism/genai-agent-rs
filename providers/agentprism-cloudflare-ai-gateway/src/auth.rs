//! Cloudflare AI Gateway authentication and endpoint materialization.

use agentprism_ai::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyResolveRequest, AuthAnswer, AuthError, AuthInteraction,
    AuthPrompt, AuthResolver, AuthSource, CancellationToken, LocalApiKeyAuth,
    LocalApiKeyResolveRequest, LocalAuthInteraction, LocalAuthResolver, LocalBoxFuture,
    LocalProviderAuthResolver, LocalResolveAuthRequest, ProviderAuthResolver, ResolveAuthRequest,
    ResolvedAuth, SecretString, SendBoxFuture,
};
use http::{HeaderMap, HeaderValue};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

const ACCOUNT_HEADER: &str = "x-pi-cloudflare-account-id";
const GATEWAY_HEADER: &str = "x-pi-cloudflare-gateway-id";

pub(crate) fn send_auth() -> Arc<dyn AuthResolver> {
    Arc::new(CloudflareResolver {
        inner: ProviderAuthResolver::new(Some(Arc::new(CloudflareKeyAuth)), None),
    })
}

pub(crate) fn local_auth() -> Rc<dyn LocalAuthResolver> {
    Rc::new(LocalCloudflareResolver {
        inner: LocalProviderAuthResolver::new(Some(Rc::new(CloudflareKeyAuth)), None),
    })
}

struct CloudflareResolver {
    inner: ProviderAuthResolver,
}

impl AuthResolver for CloudflareResolver {
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
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            materialize_endpoint(model_url.as_ref(), &mut resolved)?;
            Ok(Some(resolved))
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

struct LocalCloudflareResolver {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalCloudflareResolver {
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
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            materialize_endpoint(model_url.as_ref(), &mut resolved)?;
            Ok(Some(resolved))
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
struct CloudflareKeyAuth;

impl ApiKeyAuth for CloudflareKeyAuth {
    fn name(&self) -> &str {
        "Cloudflare API key"
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
                cancellation.clone(),
            )
            .await?;
            let gateway = prompt_send(
                interaction.as_ref(),
                false,
                "Enter Cloudflare AI Gateway ID",
                cancellation,
            )
            .await?;
            Ok(credential(key, account, gateway))
        })
    }
}

impl LocalApiKeyAuth for CloudflareKeyAuth {
    fn name(&self) -> &str {
        "Cloudflare API key"
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
                cancellation.clone(),
            )
            .await?;
            let gateway = prompt_local(
                interaction.as_ref(),
                false,
                "Enter Cloudflare AI Gateway ID",
                cancellation,
            )
            .await?;
            Ok(credential(key, account, gateway))
        })
    }
}

async fn resolve_send(
    request: ApiKeyResolveRequest,
    cancellation: CancellationToken,
) -> Result<Option<ResolvedAuth>, AuthError> {
    let key = value_send("CLOUDFLARE_API_KEY", &request, cancellation.clone()).await?;
    let account = value_send("CLOUDFLARE_ACCOUNT_ID", &request, cancellation.clone()).await?;
    let gateway = value_send("CLOUDFLARE_GATEWAY_ID", &request, cancellation).await?;
    resolved(key, account, gateway, request.credential.is_some())
}

async fn resolve_local(
    request: LocalApiKeyResolveRequest,
    cancellation: CancellationToken,
) -> Result<Option<ResolvedAuth>, AuthError> {
    let key = value_local("CLOUDFLARE_API_KEY", &request, cancellation.clone()).await?;
    let account = value_local("CLOUDFLARE_ACCOUNT_ID", &request, cancellation.clone()).await?;
    let gateway = value_local("CLOUDFLARE_GATEWAY_ID", &request, cancellation).await?;
    resolved(key, account, gateway, request.credential.is_some())
}

fn resolved(
    key: Option<String>,
    account: Option<String>,
    gateway: Option<String>,
    stored: bool,
) -> Result<Option<ResolvedAuth>, AuthError> {
    let (Some(key), Some(account), Some(gateway)) =
        (nonempty(key), nonempty(account), nonempty(gateway))
    else {
        return Ok(None);
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        "cf-aig-authorization",
        HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| AuthError::new("cloudflare_auth", "invalid Cloudflare API key"))?,
    );
    let mut transport_headers = HeaderMap::new();
    insert(&mut transport_headers, ACCOUNT_HEADER, &account)?;
    insert(&mut transport_headers, GATEWAY_HEADER, &gateway)?;
    Ok(Some(ResolvedAuth {
        api_key: None,
        headers,
        transport_headers,
        base_url: None,
        source: AuthSource::new(if stored {
            "stored credential"
        } else {
            "CLOUDFLARE_API_KEY"
        }),
    }))
}

fn materialize_endpoint(
    model_url: Option<&url::Url>,
    resolved: &mut ResolvedAuth,
) -> Result<(), AuthError> {
    let account = take(&mut resolved.transport_headers, ACCOUNT_HEADER);
    let gateway = take(&mut resolved.transport_headers, GATEWAY_HEADER);
    let (Some(model_url), Some(account), Some(gateway)) = (model_url, account, gateway) else {
        return Ok(());
    };
    let value = model_url
        .as_str()
        .replace("%7BCLOUDFLARE_ACCOUNT_ID%7D", &account)
        .replace("%7bCLOUDFLARE_ACCOUNT_ID%7d", &account)
        .replace("%7BCLOUDFLARE_GATEWAY_ID%7D", &gateway)
        .replace("%7bCLOUDFLARE_GATEWAY_ID%7d", &gateway);
    resolved.base_url = Some(url::Url::parse(&value).map_err(|_| {
        AuthError::new(
            "cloudflare_endpoint",
            "invalid resolved Cloudflare endpoint",
        )
    })?);
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

fn credential(key: String, account: String, gateway: String) -> ApiKeyCredential {
    ApiKeyCredential {
        key: Some(SecretString::new(key)),
        environment: BTreeMap::from([
            ("CLOUDFLARE_ACCOUNT_ID".into(), account),
            ("CLOUDFLARE_GATEWAY_ID".into(), gateway),
        ]),
    }
}

async fn prompt_send(
    interaction: &dyn AuthInteraction,
    secret: bool,
    message: &str,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    let prompt = prompt(secret, message);
    required_answer(interaction.prompt(prompt, cancellation).await?)
}

async fn prompt_local(
    interaction: &dyn LocalAuthInteraction,
    secret: bool,
    message: &str,
    cancellation: CancellationToken,
) -> Result<String, AuthError> {
    let prompt = prompt(secret, message);
    required_answer(interaction.prompt(prompt, cancellation).await?)
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

fn insert(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), AuthError> {
    headers.insert(
        http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AuthError::new("cloudflare_auth", "invalid private header"))?,
        HeaderValue::from_str(value)
            .map_err(|_| AuthError::new("cloudflare_auth", "invalid Cloudflare field"))?,
    );
    Ok(())
}

fn take(headers: &mut HeaderMap, name: &str) -> Option<String> {
    headers
        .remove(name)
        .and_then(|value| value.to_str().ok().map(str::to_owned))
}
