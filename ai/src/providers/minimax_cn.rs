use super::{minimax_cn_models::MINIMAX_CN_MODELS, static_provider};
use crate::api::anthropic_messages::anthropic_messages_api;
use crate::auth::{ProviderAuth, env_api_key_auth};
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn minimax_cn_provider() -> ProviderRef {
    static_provider(
        "minimax-cn",
        "MiniMax CN",
        "https://api.minimaxi.com/anthropic",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "MiniMax CN API key",
                vec!["MINIMAX_CN_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &MINIMAX_CN_MODELS,
        Arc::new(anthropic_messages_api()),
    )
}
