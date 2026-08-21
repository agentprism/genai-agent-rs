use crate::auth::helpers::OAuthLoader;
use crate::auth::oauth::anthropic::anthropic_oauth;
use crate::auth::oauth::github_copilot::github_copilot_oauth;
use crate::auth::oauth::kimi_coding::kimi_coding_oauth;
use crate::auth::oauth::openai_codex::openai_codex_oauth;
use crate::auth::oauth::openrouter::openrouter_oauth;
use crate::auth::oauth::radius::{RadiusOAuthOptions, create_radius_oauth};
use crate::auth::oauth::xai::xai_oauth;
use crate::auth::types::{AuthFuture, OAuthAuth};
use std::sync::{Arc, OnceLock, RwLock};

pub type RadiusOAuthLoader = Arc<dyn Fn(RadiusOAuthOptions) -> AuthFuture<OAuthAuth> + Send + Sync>;

pub struct OAuthFlowLoaders {
    pub anthropic: OAuthLoader,
    pub openai_codex: OAuthLoader,
    pub github_copilot: OAuthLoader,
    pub openrouter: OAuthLoader,
    pub kimi_coding: OAuthLoader,
    pub xai: OAuthLoader,
    pub radius: RadiusOAuthLoader,
}

static BUNDLED_LOADERS: OnceLock<RwLock<Option<OAuthFlowLoaders>>> = OnceLock::new();

fn bundled_loaders() -> &'static RwLock<Option<OAuthFlowLoaders>> {
    BUNDLED_LOADERS.get_or_init(|| RwLock::new(None))
}

pub fn register_bundled_oauth_flow_loaders(loaders: OAuthFlowLoaders) {
    *bundled_loaders()
        .write()
        .expect("bundled OAuth loader lock poisoned") = Some(loaders);
}

fn load_registered(
    select: impl FnOnce(&OAuthFlowLoaders) -> &OAuthLoader,
) -> Option<AuthFuture<OAuthAuth>> {
    let loader = bundled_loaders()
        .read()
        .expect("bundled OAuth loader lock poisoned")
        .as_ref()
        .map(select)
        .cloned();
    loader.map(|loader| loader())
}

pub fn load_anthropic_oauth() -> AuthFuture<OAuthAuth> {
    load_registered(|loaders| &loaders.anthropic)
        .unwrap_or_else(|| Box::pin(async { Ok(anthropic_oauth()) }))
}

pub fn load_openai_codex_oauth() -> AuthFuture<OAuthAuth> {
    load_registered(|loaders| &loaders.openai_codex)
        .unwrap_or_else(|| Box::pin(async { Ok(openai_codex_oauth()) }))
}

pub fn load_github_copilot_oauth() -> AuthFuture<OAuthAuth> {
    load_registered(|loaders| &loaders.github_copilot)
        .unwrap_or_else(|| Box::pin(async { Ok(github_copilot_oauth()) }))
}

pub fn load_openrouter_oauth() -> AuthFuture<OAuthAuth> {
    load_registered(|loaders| &loaders.openrouter)
        .unwrap_or_else(|| Box::pin(async { Ok(openrouter_oauth()) }))
}

pub fn load_kimi_coding_oauth() -> AuthFuture<OAuthAuth> {
    load_registered(|loaders| &loaders.kimi_coding)
        .unwrap_or_else(|| Box::pin(async { Ok(kimi_coding_oauth()) }))
}

pub fn load_xai_oauth() -> AuthFuture<OAuthAuth> {
    load_registered(|loaders| &loaders.xai).unwrap_or_else(|| Box::pin(async { Ok(xai_oauth()) }))
}

pub fn load_radius_oauth(options: RadiusOAuthOptions) -> AuthFuture<OAuthAuth> {
    let loader = bundled_loaders()
        .read()
        .expect("bundled OAuth loader lock poisoned")
        .as_ref()
        .map(|loaders| loaders.radius.clone());
    if let Some(loader) = loader {
        return loader(options);
    }
    Box::pin(async move { Ok(create_radius_oauth(options)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ports pi `src/auth/oauth/load.ts:35-68` without JavaScript-only dynamic imports.
    #[tokio::test]
    async fn every_flow_loader_returns_the_named_static_flow() {
        assert_eq!(
            load_anthropic_oauth().await.expect("flow").name,
            "Anthropic (Claude Pro/Max)"
        );
        assert_eq!(
            load_openai_codex_oauth().await.expect("flow").name,
            "OpenAI (ChatGPT Plus/Pro)"
        );
        assert_eq!(
            load_github_copilot_oauth().await.expect("flow").name,
            "GitHub Copilot"
        );
        assert_eq!(
            load_openrouter_oauth().await.expect("flow").name,
            "OpenRouter OAuth"
        );
        assert_eq!(
            load_kimi_coding_oauth().await.expect("flow").name,
            "Kimi Code (subscription)"
        );
        assert_eq!(
            load_xai_oauth().await.expect("flow").name,
            "xAI (Grok/X subscription)"
        );
        assert_eq!(
            load_radius_oauth(RadiusOAuthOptions {
                name: "Private Radius".to_owned(),
                gateway: "radius.example".to_owned(),
            })
            .await
            .expect("flow")
            .name,
            "Private Radius"
        );
    }
}
