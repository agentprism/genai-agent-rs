//! OpenCode Zen provider leaf crate.
#![deny(missing_docs)]
pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

fn load_catalog() -> Result<Vec<agentprism_ai::ModelDescriptor>, String> {
    let source = include_str!("../data/models.json");
    let mut models = agentprism_anthropic::parse_anthropic_published_catalog(source)
        .map_err(|error| error.to_string())?;
    models.extend(
        agentprism_provider_common::parse_google_published_catalog(source, "opencode")
            .map_err(|error| error.to_string())?,
    );
    for api in ["openai-completions", "openai-responses"] {
        models.extend(
            agentprism_openai::parse_openai_published_catalog(source, "opencode", api)
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(models)
}

agentprism_provider_common::define_static_provider! {
    id: "opencode", name: "OpenCode Zen", auth_name: "OpenCode API key", env: "OPENCODE_API_KEY",
    catalog: load_catalog(),
    send_apis: [
        ("anthropic-messages", agentprism_anthropic::anthropic_messages_api(inputs.http.clone())),
        ("google-generative-ai", agentprism_google::google_generative_ai_api(inputs.http.clone())),
        ("openai-completions", agentprism_openai::openai_completions_api(inputs.http.clone())),
        ("openai-responses", agentprism_openai::openai_responses_api(inputs.http.clone()))
    ],
    local_apis: [
        ("anthropic-messages", agentprism_anthropic::local_anthropic_messages_api(inputs.http.clone())),
        ("google-generative-ai", agentprism_google::local_google_generative_ai_api(inputs.http.clone())),
        ("openai-completions", agentprism_openai::local_openai_completions_api(inputs.http.clone())),
        ("openai-responses", agentprism_openai::local_openai_responses_api(inputs.http.clone()))
    ]
}
