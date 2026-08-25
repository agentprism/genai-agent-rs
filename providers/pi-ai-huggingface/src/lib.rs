//! Hugging Face provider leaf crate.
#![deny(missing_docs)]
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pi_ai_provider_common::define_static_provider! {
    id: "huggingface", name: "Hugging Face", auth_name: "Hugging Face token", env: "HF_TOKEN",
    catalog: pi_ai_openai::parse_openai_published_catalog(include_str!("../data/models.json"), "huggingface", "openai-completions"),
    send_apis: [("openai-completions", pi_ai_openai::openai_completions_api(inputs.http.clone()))],
    local_apis: [("openai-completions", pi_ai_openai::local_openai_completions_api(inputs.http.clone()))]
}
