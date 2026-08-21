use super::{openrouter_models::OPENROUTER_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::{env_api_key_auth, lazy_oauth};
use crate::auth::oauth::load::load_openrouter_oauth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn openrouter_provider() -> ProviderRef {
    static_provider(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai/api/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "OpenRouter API key",
                vec!["OPENROUTER_API_KEY".to_owned()],
            )),
            oauth: Some(lazy_oauth(
                "OpenRouter OAuth".to_owned(),
                None,
                Some("Sign in with OpenRouter".to_owned()),
                Arc::new(load_openrouter_oauth),
            )),
        },
        &OPENROUTER_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
