use super::{groq_models::GROQ_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn groq_provider() -> ProviderRef {
    static_provider(
        "groq",
        "Groq",
        "https://api.groq.com/openai/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Groq API key",
                vec!["GROQ_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &GROQ_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
