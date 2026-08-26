//! Z.AI Coding CN provider leaf crate.
#![deny(missing_docs)]
pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
agentprism_provider_common::define_static_provider! {
    id: "zai-coding-cn", name: "Z.AI Coding CN", auth_name: "Z.AI Coding CN API key", env: "ZAI_CODING_CN_API_KEY",
    catalog: agentprism_openai::parse_openai_published_catalog(include_str!("../data/models.json"), "zai-coding-cn", "openai-completions"),
    send_apis: [("openai-completions", agentprism_openai::openai_completions_api(inputs.http.clone()))],
    local_apis: [("openai-completions", agentprism_openai::local_openai_completions_api(inputs.http.clone()))]
}
