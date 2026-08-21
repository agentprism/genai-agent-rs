use super::{static_provider, xai_models::XAI_MODELS};
use crate::api::openai_responses::open_ai_responses_api;
use crate::auth::helpers::{env_api_key_auth, lazy_oauth};
use crate::auth::oauth::load::load_xai_oauth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn xai_provider() -> ProviderRef {
    static_provider(
        "xai",
        "xAI",
        "https://api.x.ai/v1",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "xAI API key",
                vec!["XAI_API_KEY".to_owned()],
            )),
            oauth: Some(lazy_oauth(
                "xAI (Grok/X subscription)".to_owned(),
                Some(true),
                Some("Sign in with SuperGrok or X Premium".to_owned()),
                Arc::new(load_xai_oauth),
            )),
        },
        &XAI_MODELS,
        Arc::new(open_ai_responses_api()),
    )
}
