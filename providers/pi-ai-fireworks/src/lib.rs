//! Fireworks provider leaf crate.
#![deny(missing_docs)]
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

fn load_catalog() -> Result<Vec<pi_ai::ModelDescriptor>, String> {
    let source = include_str!("../data/models.json");
    let mut models = pi_ai_anthropic::parse_anthropic_published_catalog(source)
        .map_err(|error| error.to_string())?;
    models.extend(
        pi_ai_openai::parse_openai_published_catalog(source, "fireworks", "openai-completions")
            .map_err(|error| error.to_string())?,
    );
    Ok(models)
}

pi_ai_provider_common::define_static_provider! {
    id: "fireworks", name: "Fireworks", auth_name: "Fireworks API key", env: "FIREWORKS_API_KEY",
    catalog: load_catalog(),
    send_apis: [
        ("anthropic-messages", pi_ai_anthropic::anthropic_messages_api(inputs.http.clone())),
        ("openai-completions", pi_ai_openai::openai_completions_api(inputs.http.clone()))
    ],
    local_apis: [
        ("anthropic-messages", pi_ai_anthropic::local_anthropic_messages_api(inputs.http.clone())),
        ("openai-completions", pi_ai_openai::local_openai_completions_api(inputs.http.clone()))
    ]
}
