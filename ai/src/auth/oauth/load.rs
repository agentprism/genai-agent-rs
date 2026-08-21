use crate::auth::helpers::OAuthLoader;
use crate::auth::types::{AuthError, AuthFuture, OAuthAuth};
use std::sync::OnceLock;

pub struct OAuthFlowLoaders {
    pub openai_codex: OAuthLoader,
    pub openrouter: OAuthLoader,
    pub xai: OAuthLoader,
}

static BUNDLED_LOADERS: OnceLock<OAuthFlowLoaders> = OnceLock::new();

pub fn register_bundled_oauth_flow_loaders(
    loaders: OAuthFlowLoaders,
) -> Result<(), OAuthFlowLoaders> {
    BUNDLED_LOADERS.set(loaders)
}

fn load(
    select: impl FnOnce(&OAuthFlowLoaders) -> &OAuthLoader,
    provider: &'static str,
) -> AuthFuture<OAuthAuth> {
    let loader = BUNDLED_LOADERS.get().map(select).cloned();
    Box::pin(async move {
        match loader {
            Some(loader) => loader().await,
            None => Err(AuthError::new(format!(
                "Bundled OAuth flow is not registered for {provider}"
            ))),
        }
    })
}

pub fn load_openai_codex_oauth() -> AuthFuture<OAuthAuth> {
    load(|loaders| &loaders.openai_codex, "openai-codex")
}

pub fn load_openrouter_oauth() -> AuthFuture<OAuthAuth> {
    load(|loaders| &loaders.openrouter, "openrouter")
}

pub fn load_xai_oauth() -> AuthFuture<OAuthAuth> {
    load(|loaders| &loaders.xai, "xai")
}
