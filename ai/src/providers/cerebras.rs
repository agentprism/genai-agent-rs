use super::{cerebras_models::CEREBRAS_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn cerebras_provider() -> ProviderRef {
    static_provider(
        "cerebras",
        "Cerebras",
        "https://api.cerebras.ai/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Cerebras API key",
                vec!["CEREBRAS_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &CEREBRAS_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
