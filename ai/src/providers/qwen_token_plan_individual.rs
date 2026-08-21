use super::{
    qwen_token_plan_individual_models::QWEN_TOKEN_PLAN_INDIVIDUAL_MODELS, static_provider,
};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn qwen_token_plan_individual_provider() -> ProviderRef {
    static_provider(
        "qwen-token-plan-individual",
        "Qwen Token Plan Individual",
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Qwen Token Plan Individual API key",
                vec!["QWEN_TOKEN_PLAN_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &QWEN_TOKEN_PLAN_INDIVIDUAL_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
