use super::{static_provider, zai_models::ZAI_MODELS};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn zai_provider() -> ProviderRef {
    static_provider(
        "zai",
        "Z.AI",
        "https://api.z.ai/api/coding/paas/v4",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Z.AI API key",
                vec!["ZAI_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &ZAI_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
