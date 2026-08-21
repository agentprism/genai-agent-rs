use super::{baseten_models::BASETEN_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn baseten_provider() -> ProviderRef {
    static_provider(
        "baseten",
        "Baseten",
        "https://inference.baseten.co/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Baseten API key",
                vec!["BASETEN_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &BASETEN_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
