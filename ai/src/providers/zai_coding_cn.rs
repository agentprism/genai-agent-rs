use super::{static_provider, zai_coding_cn_models::ZAI_CODING_CN_MODELS};
use crate::api::openai_completions::open_ai_completions_api;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn zai_coding_cn_provider() -> ProviderRef {
    static_provider(
        "zai-coding-cn",
        "Z.AI Coding CN",
        "https://open.bigmodel.cn/api/coding/paas/v4",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Z.AI Coding CN API key",
                vec!["ZAI_CODING_CN_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &ZAI_CODING_CN_MODELS,
        Arc::new(open_ai_completions_api()),
    )
}
