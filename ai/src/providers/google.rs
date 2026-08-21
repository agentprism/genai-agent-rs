use super::{google_models::GOOGLE_MODELS, static_provider};
use crate::api::google_generative_ai::google_generative_ai_api;
use crate::auth::{ProviderAuth, env_api_key_auth};
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn google_provider() -> ProviderRef {
    static_provider(
        "google",
        "Google",
        "https://generativelanguage.googleapis.com/v1beta",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Gemini API key",
                vec!["GEMINI_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &GOOGLE_MODELS,
        Arc::new(google_generative_ai_api()),
    )
}
