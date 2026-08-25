//! Xiaomi Token Plan AMS provider leaf crate.
#![deny(missing_docs)]
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pi_ai_provider_common::define_static_provider! {
    id: "xiaomi-token-plan-ams", name: "Xiaomi Token Plan AMS", auth_name: "Xiaomi Token Plan AMS API key", env: "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
    catalog: pi_ai_openai::parse_openai_published_catalog(include_str!("../data/models.json"), "xiaomi-token-plan-ams", "openai-completions"),
    send_apis: [("openai-completions", pi_ai_openai::openai_completions_api(inputs.http.clone()))],
    local_apis: [("openai-completions", pi_ai_openai::local_openai_completions_api(inputs.http.clone()))]
}
