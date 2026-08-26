//! Xiaomi Token Plan AMS provider leaf crate.
#![deny(missing_docs)]
pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
agentprism_provider_common::define_static_provider! {
    id: "xiaomi-token-plan-ams", name: "Xiaomi Token Plan AMS", auth_name: "Xiaomi Token Plan AMS API key", env: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
    catalog: agentprism_openai::parse_openai_published_catalog(include_str!("../data/models.json"), "xiaomi-token-plan-ams", "openai-completions"),
    send_apis: [("openai-completions", agentprism_openai::openai_completions_api(inputs.http.clone()))],
    local_apis: [("openai-completions", agentprism_openai::local_openai_completions_api(inputs.http.clone()))]
}
