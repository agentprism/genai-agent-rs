use super::{minimax_models::MINIMAX_MODELS, static_provider};
use crate::api::anthropic_messages::anthropic_messages_api;
use crate::auth::{ProviderAuth, env_api_key_auth};
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn minimax_provider() -> ProviderRef {
    static_provider(
        "minimax",
        "MiniMax",
        "https://api.minimax.io/anthropic",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "MiniMax API key",
                vec!["MINIMAX_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &MINIMAX_MODELS,
        Arc::new(anthropic_messages_api()),
    )
}
