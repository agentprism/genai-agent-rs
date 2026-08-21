use super::{ant_ling_models::ANT_LING_MODELS, static_provider};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn ant_ling_provider() -> ProviderRef {
    static_provider(
        "ant-ling",
        "Ant Ling",
        "https://api.ant-ling.com/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Ant Ling API key",
                vec!["ANT_LING_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &ANT_LING_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
