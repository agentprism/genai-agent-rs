use super::{moonshotai_models::MOONSHOTAI_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn moonshotai_provider() -> ProviderRef {
    static_provider(
        "moonshotai",
        "Moonshot AI",
        "https://api.moonshot.ai/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Moonshot AI API key",
                vec!["MOONSHOT_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &MOONSHOTAI_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
