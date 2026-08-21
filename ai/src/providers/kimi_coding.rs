use super::{kimi_coding_models::KIMI_CODING_MODELS, static_provider};
use crate::api::anthropic_messages::anthropic_messages_api;
use crate::auth::oauth::load::load_kimi_coding_oauth;
use crate::auth::{ProviderAuth, env_api_key_auth, lazy_oauth};
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn kimi_coding_provider() -> ProviderRef {
    static_provider(
        "kimi-coding",
        "Kimi For Coding",
        "https://api.kimi.com/coding",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Kimi API key",
                vec!["KIMI_API_KEY".to_owned()],
            )),
            oauth: Some(lazy_oauth(
                "Kimi Code (subscription)".to_owned(),
                Some(true),
                Some("Sign in with Kimi Code".to_owned()),
                Arc::new(load_kimi_coding_oauth),
            )),
        },
        &KIMI_CODING_MODELS,
        Arc::new(anthropic_messages_api()),
    )
}
