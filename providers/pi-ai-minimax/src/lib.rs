//! MiniMax provider leaf crate.
#![deny(missing_docs)]
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pi_ai_provider_common::define_static_provider! {
    id: "minimax", name: "MiniMax", auth_name: "MiniMax API key", env: "MINIMAX_API_KEY",
    catalog: pi_ai_anthropic::parse_anthropic_published_catalog(include_str!("../data/models.json")),
    send_apis: [("anthropic-messages", pi_ai_anthropic::anthropic_messages_api(inputs.http.clone()))],
    local_apis: [("anthropic-messages", pi_ai_anthropic::local_anthropic_messages_api(inputs.http.clone()))]
}
