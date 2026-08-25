//! MiniMax CN provider leaf crate.
#![deny(missing_docs)]
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pi_ai_provider_common::define_static_provider! {
    id: "minimax-cn", name: "MiniMax CN", auth_name: "MiniMax CN API key", env: "MINIMAX_CN_API_KEY",
    catalog: pi_ai_anthropic::parse_anthropic_published_catalog(include_str!("../data/models.json")),
    send_apis: [("anthropic-messages", pi_ai_anthropic::anthropic_messages_api(inputs.http.clone()))],
    local_apis: [("anthropic-messages", pi_ai_anthropic::local_anthropic_messages_api(inputs.http.clone()))]
}
