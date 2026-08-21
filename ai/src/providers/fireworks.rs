use super::fireworks_models::FIREWORKS_MODELS;
use crate::api::ProviderStreams;
use crate::api::anthropic_messages::anthropic_messages_api;
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::{ProviderAuth, env_api_key_auth};
use crate::models::{CreateProviderOptions, ProviderApi, ProviderRef, create_provider};
use crate::types::Api;
use indexmap::IndexMap;
use std::sync::Arc;

pub fn fireworks_provider() -> ProviderRef {
    let anthropic: Arc<dyn ProviderStreams> = Arc::new(anthropic_messages_api());
    let completions: Arc<dyn ProviderStreams> = Arc::new(open_ai_completions_api());
    create_provider(CreateProviderOptions {
        id: "fireworks".to_owned(),
        name: Some("Fireworks".to_owned()),
        base_url: Some("https://api.fireworks.ai/inference".to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Fireworks API key",
                vec!["FIREWORKS_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        models: FIREWORKS_MODELS.values().cloned().collect(),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::ByApi(IndexMap::from([
            (Api::from("anthropic-messages"), anthropic),
            (Api::from("openai-completions"), completions),
        ])),
    })
}
