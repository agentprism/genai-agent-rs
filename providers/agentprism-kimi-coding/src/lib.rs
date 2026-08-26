//! Kimi For Coding provider leaf crate with concrete device OAuth.

#![deny(missing_docs)]

mod oauth;

use agentprism_ai::ApiId;
use std::rc::Rc;
use std::sync::Arc;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pub use oauth::{KimiCodingOAuth, LocalKimiCodingOAuth};

/// Returns the pinned Kimi For Coding catalog owned by this leaf.
pub fn models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    agentprism_anthropic::parse_anthropic_published_catalog(include_str!("../data/models.json"))
        .map_err(ProviderBuildError::catalog)
}

/// Builds the Send Kimi For Coding registration with its concrete OAuth flow.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let oauth = Arc::new(
        KimiCodingOAuth::from_environment(inputs.http.clone(), &inputs.environment)
            .map_err(ProviderBuildError::configuration)?,
    );
    agentprism_provider_common::build_provider(
        "kimi-coding",
        "Kimi For Coding",
        models()?,
        agentprism_provider_common::family_auth("Kimi API key", "KIMI_API_KEY", Some(oauth)),
        [(
            ApiId::new("anthropic-messages"),
            agentprism_anthropic::anthropic_messages_api(inputs.http)
                as Arc<dyn agentprism_ai::ChatApi>,
        )],
    )
}

/// Builds the local Kimi For Coding registration with concrete OAuth.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let oauth = Rc::new(
        LocalKimiCodingOAuth::from_environment(inputs.http.clone(), &inputs.environment)
            .map_err(ProviderBuildError::configuration)?,
    );
    agentprism_provider_common::build_local_provider(
        "kimi-coding",
        "Kimi For Coding",
        models()?,
        agentprism_provider_common::local_family_auth("Kimi API key", "KIMI_API_KEY", Some(oauth)),
        [(
            ApiId::new("anthropic-messages"),
            agentprism_anthropic::local_anthropic_messages_api(inputs.http)
                as Rc<dyn agentprism_ai::LocalChatApi>,
        )],
    )
}
