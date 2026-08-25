//! Kimi For Coding provider leaf crate with concrete device OAuth.

#![deny(missing_docs)]

mod oauth;

use pi_ai::ApiId;
use std::rc::Rc;
use std::sync::Arc;

pub use oauth::{KimiCodingOAuth, LocalKimiCodingOAuth};
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

/// Returns the pinned Kimi For Coding catalog owned by this leaf.
pub fn models() -> Result<Vec<pi_ai::ModelDescriptor>, ProviderBuildError> {
    pi_ai_anthropic::parse_anthropic_published_catalog(include_str!("../data/models.json"))
        .map_err(ProviderBuildError::catalog)
}

/// Builds the Send Kimi For Coding registration with its concrete OAuth flow.
pub fn provider(inputs: ProviderInputs) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    let oauth = Arc::new(
        KimiCodingOAuth::from_environment(inputs.http.clone(), &inputs.environment)
            .map_err(ProviderBuildError::configuration)?,
    );
    pi_ai_provider_common::build_provider(
        "kimi-coding",
        "Kimi For Coding",
        models()?,
        pi_ai_provider_common::family_auth("Kimi API key", "KIMI_API_KEY", Some(oauth)),
        [(
            ApiId::new("anthropic-messages"),
            pi_ai_anthropic::anthropic_messages_api(inputs.http) as Arc<dyn pi_ai::ChatApi>,
        )],
    )
}

/// Builds the local Kimi For Coding registration with concrete OAuth.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    let oauth = Rc::new(
        LocalKimiCodingOAuth::from_environment(inputs.http.clone(), &inputs.environment)
            .map_err(ProviderBuildError::configuration)?,
    );
    pi_ai_provider_common::build_local_provider(
        "kimi-coding",
        "Kimi For Coding",
        models()?,
        pi_ai_provider_common::local_family_auth("Kimi API key", "KIMI_API_KEY", Some(oauth)),
        [(
            ApiId::new("anthropic-messages"),
            pi_ai_anthropic::local_anthropic_messages_api(inputs.http)
                as Rc<dyn pi_ai::LocalChatApi>,
        )],
    )
}
