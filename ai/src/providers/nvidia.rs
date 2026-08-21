use super::{nvidia_models::NVIDIA_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn nvidia_provider() -> ProviderRef {
    static_provider(
        "nvidia",
        "NVIDIA",
        "https://integrate.api.nvidia.com/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "NVIDIA API key",
                vec!["NVIDIA_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &NVIDIA_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
