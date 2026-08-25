//! Qwen Token Plan CN provider leaf crate.
#![deny(missing_docs)]
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pi_ai_provider_common::define_static_provider! {
    id: "qwen-token-plan-cn", name: "Qwen Token Plan CN", auth_name: "Qwen Token Plan CN API key", env: "QWEN_TOKEN_PLAN_CN_API_KEY",
    catalog: pi_ai_openai::parse_openai_published_catalog(include_str!("../data/models.json"), "qwen-token-plan-cn", "openai-completions"),
    send_apis: [("openai-completions", pi_ai_openai::openai_completions_api(inputs.http.clone()))],
    local_apis: [("openai-completions", pi_ai_openai::local_openai_completions_api(inputs.http.clone()))]
}
