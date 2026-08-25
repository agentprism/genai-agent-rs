//! NVIDIA provider leaf crate.
#![deny(missing_docs)]
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pi_ai_provider_common::define_static_provider! {
    id: "nvidia", name: "NVIDIA", auth_name: "NVIDIA API key", env: "NVIDIA_API_KEY",
    catalog: pi_ai_openai::parse_openai_published_catalog(include_str!("../data/models.json"), "nvidia", "openai-completions"),
    send_apis: [("openai-completions", pi_ai_openai::openai_completions_api(inputs.http.clone()))],
    local_apis: [("openai-completions", pi_ai_openai::local_openai_completions_api(inputs.http.clone()))]
}
