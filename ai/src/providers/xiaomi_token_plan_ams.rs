use super::{static_provider, xiaomi_token_plan_ams_models::XIAOMI_TOKEN_PLAN_AMS_MODELS};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn xiaomi_token_plan_ams_provider() -> ProviderRef {
    static_provider(
        "xiaomi-token-plan-ams",
        "Xiaomi Token Plan AMS",
        "https://token-plan-ams.xiaomimimo.com/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Xiaomi Token Plan AMS API key",
                vec!["XIAOMI_TOKEN_PLAN_AMS_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &XIAOMI_TOKEN_PLAN_AMS_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
