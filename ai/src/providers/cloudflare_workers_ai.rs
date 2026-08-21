use super::cloudflare_auth::cloudflare_workers_ai_auth;
use super::cloudflare_stream::cloudflare_streams;
use super::cloudflare_workers_ai_models::CLOUDFLARE_WORKERS_AI_MODELS;
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::types::ProviderAuth;
use crate::models::{CreateProviderOptions, ProviderApi, ProviderRef, create_provider};
use std::sync::Arc;

pub fn cloudflare_workers_ai_provider() -> ProviderRef {
    create_provider(CreateProviderOptions {
        id: "cloudflare-workers-ai".to_owned(),
        name: Some("Cloudflare Workers AI".to_owned()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(cloudflare_workers_ai_auth()),
            oauth: None,
        },
        models: CLOUDFLARE_WORKERS_AI_MODELS.values().cloned().collect(),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(cloudflare_streams(Arc::new(open_ai_completions_api()))),
    })
}
