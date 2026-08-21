use super::{deepseek_models::DEEPSEEK_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn deepseek_provider() -> ProviderRef {
    static_provider(
        "deepseek",
        "DeepSeek",
        "https://api.deepseek.com",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "DeepSeek API key",
                vec!["DEEPSEEK_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &DEEPSEEK_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
