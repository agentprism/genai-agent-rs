//! xAI provider leaf crate with concrete RFC 8628 OAuth.

#![deny(missing_docs)]

mod oauth;

use agentprism_ai::ApiId;
use std::rc::Rc;
use std::sync::Arc;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pub use oauth::{LocalXaiOAuth, XaiOAuth};

/// Returns the pinned xAI catalog owned by this leaf.
pub fn models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    agentprism_openai::parse_openai_published_catalog(
        include_str!("../data/models.json"),
        "xai",
        "openai-responses",
    )
    .map_err(ProviderBuildError::catalog)
}

/// Builds the Send xAI registration with concrete OAuth.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let oauth = Arc::new(XaiOAuth::new(inputs.http.clone()));
    agentprism_provider_common::build_provider(
        "xai",
        "xAI",
        models()?,
        agentprism_provider_common::bearer_auth("xAI API key", "XAI_API_KEY", Some(oauth)),
        [(
            ApiId::new("openai-responses"),
            agentprism_openai::openai_responses_api(inputs.http) as Arc<dyn agentprism_ai::ChatApi>,
        )],
    )
}

/// Builds the local xAI registration with concrete OAuth.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let oauth = Rc::new(LocalXaiOAuth::new(inputs.http.clone()));
    agentprism_provider_common::build_local_provider(
        "xai",
        "xAI",
        models()?,
        agentprism_provider_common::local_bearer_auth("xAI API key", "XAI_API_KEY", Some(oauth)),
        [(
            ApiId::new("openai-responses"),
            agentprism_openai::local_openai_responses_api(inputs.http)
                as Rc<dyn agentprism_ai::LocalChatApi>,
        )],
    )
}
