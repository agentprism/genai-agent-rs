use super::{static_provider, together_models::TOGETHER_MODELS};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn together_provider() -> ProviderRef {
    static_provider(
        "together",
        "Together",
        "https://api.together.ai/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Together API key",
                vec!["TOGETHER_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &TOGETHER_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
