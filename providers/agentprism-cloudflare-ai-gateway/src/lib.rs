//! Cloudflare AI Gateway provider leaf crate.

#![deny(missing_docs)]

mod auth;
mod binding;

use agentprism_ai::ApiId;
use std::rc::Rc;
use std::sync::Arc;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pub use binding::*;

/// Returns the pinned Cloudflare AI Gateway catalog owned by this leaf.
pub fn models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    let source = include_str!("../data/models.json");
    let mut models = agentprism_anthropic::parse_anthropic_published_catalog(source)
        .map_err(ProviderBuildError::catalog)?;
    for api in ["openai-completions", "openai-responses"] {
        models.extend(
            agentprism_openai::parse_openai_published_catalog(source, "cloudflare-ai-gateway", api)
                .map_err(ProviderBuildError::catalog)?,
        );
    }
    Ok(models)
}

/// Builds the Send Cloudflare AI Gateway registration.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let http = inputs.http;
    agentprism_provider_common::build_provider(
        "cloudflare-ai-gateway",
        "Cloudflare AI Gateway",
        models()?,
        auth::send_auth(),
        [
            (
                ApiId::new("anthropic-messages"),
                agentprism_anthropic::anthropic_messages_api(http.clone())
                    as Arc<dyn agentprism_ai::ChatApi>,
            ),
            (
                ApiId::new("openai-completions"),
                agentprism_openai::openai_completions_api(http.clone())
                    as Arc<dyn agentprism_ai::ChatApi>,
            ),
            (
                ApiId::new("openai-responses"),
                agentprism_openai::openai_responses_api(http) as Arc<dyn agentprism_ai::ChatApi>,
            ),
        ],
    )
}

/// Builds the local Cloudflare AI Gateway registration.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let http = inputs.http;
    agentprism_provider_common::build_local_provider(
        "cloudflare-ai-gateway",
        "Cloudflare AI Gateway",
        models()?,
        auth::local_auth(),
        [
            (
                ApiId::new("anthropic-messages"),
                agentprism_anthropic::local_anthropic_messages_api(http.clone())
                    as Rc<dyn agentprism_ai::LocalChatApi>,
            ),
            (
                ApiId::new("openai-completions"),
                agentprism_openai::local_openai_completions_api(http.clone())
                    as Rc<dyn agentprism_ai::LocalChatApi>,
            ),
            (
                ApiId::new("openai-responses"),
                agentprism_openai::local_openai_responses_api(http)
                    as Rc<dyn agentprism_ai::LocalChatApi>,
            ),
        ],
    )
}
