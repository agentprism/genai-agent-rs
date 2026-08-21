use super::{openai_models::OPENAI_MODELS, static_provider};
use crate::api::openai_responses::open_ai_responses_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn openai_provider() -> ProviderRef {
    static_provider(
        "openai",
        "OpenAI",
        "https://api.openai.com/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "OpenAI API key",
                vec!["OPENAI_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &OPENAI_MODELS,
        Arc::new(open_ai_responses_api()),
    )
}
