use super::{huggingface_models::HUGGINGFACE_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn huggingface_provider() -> ProviderRef {
    static_provider(
        "huggingface",
        "Hugging Face",
        "https://router.huggingface.co/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Hugging Face token",
                vec!["HF_TOKEN".to_owned()],
            )),
            oauth: None,
        },
        &HUGGINGFACE_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
