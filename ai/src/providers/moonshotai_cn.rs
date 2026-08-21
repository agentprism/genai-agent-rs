use super::{moonshotai_cn_models::MOONSHOTAI_CN_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn moonshotai_cn_provider() -> ProviderRef {
    static_provider(
        "moonshotai-cn",
        "Moonshot AI CN",
        "https://api.moonshot.cn/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Moonshot AI API key",
                vec!["MOONSHOT_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &MOONSHOTAI_CN_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
