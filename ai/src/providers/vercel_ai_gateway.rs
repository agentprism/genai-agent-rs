use super::{static_provider, vercel_ai_gateway_models::VERCEL_AI_GATEWAY_MODELS};
use crate::api::anthropic_messages::anthropic_messages_api;
use crate::auth::{ProviderAuth, env_api_key_auth};
use crate::models::ProviderRef;
use std::sync::Arc;

pub fn vercel_ai_gateway_provider() -> ProviderRef {
    static_provider(
        "vercel-ai-gateway",
        "Vercel AI Gateway",
        "https://ai-gateway.vercel.sh",
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Vercel AI Gateway API key",
                vec!["AI_GATEWAY_API_KEY".to_owned()],
            )),
            oauth: None,
        },
        &VERCEL_AI_GATEWAY_MODELS,
        Arc::new(anthropic_messages_api()),
    )
}
