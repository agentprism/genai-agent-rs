use crate::auth::oauth::anthropic::anthropic_oauth;
use crate::auth::oauth::github_copilot::github_copilot_oauth;
use crate::auth::oauth::kimi_coding::kimi_coding_oauth;
use crate::auth::oauth::load::{OAuthFlowLoaders, register_bundled_oauth_flow_loaders};
use crate::auth::oauth::openai_codex::openai_codex_oauth;
use crate::auth::oauth::openrouter::openrouter_oauth;
use crate::auth::oauth::radius::create_radius_oauth;
use crate::auth::oauth::xai::xai_oauth;
use std::sync::Arc;

pub fn register_bun_oauth_flows() {
    register_bundled_oauth_flow_loaders(OAuthFlowLoaders {
        anthropic: Arc::new(|| Box::pin(async { Ok(anthropic_oauth()) })),
        openai_codex: Arc::new(|| Box::pin(async { Ok(openai_codex_oauth()) })),
        github_copilot: Arc::new(|| Box::pin(async { Ok(github_copilot_oauth()) })),
        openrouter: Arc::new(|| Box::pin(async { Ok(openrouter_oauth()) })),
        kimi_coding: Arc::new(|| Box::pin(async { Ok(kimi_coding_oauth()) })),
        xai: Arc::new(|| Box::pin(async { Ok(xai_oauth()) })),
        radius: Arc::new(|options| Box::pin(async move { Ok(create_radius_oauth(options)) })),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::load::{load_openai_codex_oauth, load_radius_oauth};
    use crate::auth::oauth::radius::RadiusOAuthOptions;

    /// Pins pi `src/bun-oauth.ts:11-21`'s complete static registration.
    #[tokio::test]
    async fn registers_static_flow_factories_including_radius() {
        register_bun_oauth_flows();
        assert_eq!(
            load_openai_codex_oauth().await.expect("flow").name,
            "OpenAI (ChatGPT Plus/Pro)"
        );
        assert_eq!(
            load_radius_oauth(RadiusOAuthOptions {
                name: "Radius".to_owned(),
                gateway: "radius.example".to_owned(),
            })
            .await
            .expect("flow")
            .name,
            "Radius"
        );
    }
}
