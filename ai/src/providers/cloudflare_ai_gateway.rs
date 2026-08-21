use super::cloudflare_ai_gateway_models::CLOUDFLARE_AI_GATEWAY_MODELS;
use super::cloudflare_auth::cloudflare_ai_gateway_auth;
use super::cloudflare_stream::cloudflare_streams;
use crate::api::ProviderStreams;
use crate::api::anthropic_messages::anthropic_messages_api;
use crate::api::openai_completions::open_ai_completions_api;
use crate::api::openai_responses::open_ai_responses_api;
use crate::auth::ProviderAuth;
use crate::models::{CreateProviderOptions, ProviderApi, ProviderRef, create_provider};
use crate::types::Api;
use indexmap::IndexMap;
use std::sync::Arc;

pub fn cloudflare_ai_gateway_provider() -> ProviderRef {
    let anthropic: Arc<dyn ProviderStreams> =
        cloudflare_streams(Arc::new(anthropic_messages_api()));
    let completions: Arc<dyn ProviderStreams> =
        cloudflare_streams(Arc::new(open_ai_completions_api()));
    let responses: Arc<dyn ProviderStreams> = cloudflare_streams(Arc::new(open_ai_responses_api()));
    create_provider(CreateProviderOptions {
        id: "cloudflare-ai-gateway".to_owned(),
        name: Some("Cloudflare AI Gateway".to_owned()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(cloudflare_ai_gateway_auth()),
            oauth: None,
        },
        models: CLOUDFLARE_AI_GATEWAY_MODELS.values().cloned().collect(),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::ByApi(IndexMap::from([
            (Api::from("anthropic-messages"), anthropic),
            (Api::from("openai-completions"), completions),
            (Api::from("openai-responses"), responses),
        ])),
    })
}
