//! Xiaomi Token Plan CN provider leaf crate.
#![deny(missing_docs)]
pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
agentprism_provider_common::define_static_provider! {
    id: "xiaomi-token-plan-cn", name: "Xiaomi Token Plan CN", auth_name: "Xiaomi Token Plan CN API key", env: "XIAOMI_TOKEN_PLAN_CN_API_KEY",
    catalog: agentprism_openai::parse_openai_published_catalog(include_str!("../data/models.json"), "xiaomi-token-plan-cn", "openai-completions"),
    send_apis: [("openai-completions", agentprism_openai::openai_completions_api(inputs.http.clone()))],
    local_apis: [("openai-completions", agentprism_openai::local_openai_completions_api(inputs.http.clone()))]
}
