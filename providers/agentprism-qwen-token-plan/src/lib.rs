//! Qwen Token Plan provider leaf crate.
#![deny(missing_docs)]
pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
agentprism_provider_common::define_static_provider! {
    id: "qwen-token-plan", name: "Qwen Token Plan", auth_name: "Qwen Token Plan API key", env: "QWEN_TOKEN_PLAN_API_KEY",
    catalog: agentprism_openai::parse_openai_published_catalog(include_str!("../data/models.json"), "qwen-token-plan", "openai-completions"),
    send_apis: [("openai-completions", agentprism_openai::openai_completions_api(inputs.http.clone()))],
    local_apis: [("openai-completions", agentprism_openai::local_openai_completions_api(inputs.http.clone()))]
}
