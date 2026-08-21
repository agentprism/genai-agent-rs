use super::{openai_codex_models::OPENAI_CODEX_MODELS, static_provider};
use crate::api::openai_codex_responses::open_ai_codex_responses_api;
use crate::auth::helpers::lazy_oauth;
use crate::auth::oauth::load::load_openai_codex_oauth;
use crate::auth::types::ProviderAuth;
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn openai_codex_provider() -> ProviderRef {
    static_provider(
        "openai-codex",
        "OpenAI Codex",
        "https://chatgpt.com/backend-api",
        ProviderAuth {
            api_key: None,
            oauth: Some(lazy_oauth(
                "OpenAI (ChatGPT Plus/Pro)".to_owned(),
                Some(true),
                None,
                Arc::new(load_openai_codex_oauth),
            )),
        },
        &OPENAI_CODEX_MODELS,
        Arc::new(open_ai_codex_responses_api()),
    )
}
