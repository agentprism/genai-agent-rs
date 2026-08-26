//! Gemini Developer API authentication.

use agentprism_ai::{
    AuthError, AuthInteraction, AuthResolver, CancellationToken, Credential, EnvironmentApiKeyAuth,
    LocalAuthInteraction, LocalAuthResolver, LocalBoxFuture, LocalProviderAuthResolver,
    LocalResolveAuthRequest, ProviderAuthResolver, ResolveAuthRequest, ResolvedAuth, SendBoxFuture,
};
use http::HeaderValue;
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

/// Creates the Send Gemini Developer API auth resolver.
pub fn google_auth_resolver() -> Arc<dyn AuthResolver> {
    Arc::new(GoogleAuthResolver {
        inner: ProviderAuthResolver::new(
            Some(Arc::new(EnvironmentApiKeyAuth::new(
                "Gemini API key",
                ["GEMINI_API_KEY"],
            ))),
            None,
        ),
    })
}

/// Creates the local-executor Gemini Developer API auth resolver.
pub fn local_google_auth_resolver() -> Rc<dyn LocalAuthResolver> {
    Rc::new(LocalGoogleAuthResolver {
        inner: LocalProviderAuthResolver::new(
            Some(Rc::new(EnvironmentApiKeyAuth::new(
                "Gemini API key",
                ["GEMINI_API_KEY"],
            ))),
            None,
        ),
    })
}

struct GoogleAuthResolver {
    inner: ProviderAuthResolver,
}

impl AuthResolver for GoogleAuthResolver {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let custom_base_url = custom_model_base_url(
                request.model.as_ref(),
                "https://generativelanguage.googleapis.com/v1beta",
            );
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            insert_google_api_key_header(&mut resolved)?;
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

struct LocalGoogleAuthResolver {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalGoogleAuthResolver {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let custom_base_url = custom_model_base_url(
                request.model.as_ref(),
                "https://generativelanguage.googleapis.com/v1beta",
            );
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            insert_google_api_key_header(&mut resolved)?;
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
