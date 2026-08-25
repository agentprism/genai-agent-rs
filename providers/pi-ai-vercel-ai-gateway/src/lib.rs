//! Vercel AI Gateway provider leaf crate.
#![deny(missing_docs)]
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pi_ai_provider_common::define_static_provider! {
    id: "vercel-ai-gateway", name: "Vercel AI Gateway", auth_name: "Vercel AI Gateway API key", env: "AI_GATEWAY_API_KEY",
    catalog: pi_ai_anthropic::parse_anthropic_published_catalog(include_str!("../data/models.json")),
    send_apis: [("anthropic-messages", pi_ai_anthropic::anthropic_messages_api(inputs.http.clone()))],
    local_apis: [("anthropic-messages", pi_ai_anthropic::local_anthropic_messages_api(inputs.http.clone()))]
}
