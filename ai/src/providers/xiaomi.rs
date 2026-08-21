use super::{static_provider, xiaomi_models::XIAOMI_MODELS};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn xiaomi_provider() -> ProviderRef {
    static_provider(
        "xiaomi",
        "Xiaomi",
        "https://api.xiaomimimo.com/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Xiaomi API key",
                vec!["XIAOMI_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &XIAOMI_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
