//! OpenRouter provider leaf backed by the shared OpenAI Completions family.

#![deny(missing_docs)]

mod oauth;

use agentprism_ai::ApiId;
use std::rc::Rc;
use std::sync::Arc;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pub use oauth::{LocalOpenRouterOAuth, OpenRouterOAuth};

/// Returns the pinned OpenRouter catalog owned by this leaf.
pub fn models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    agentprism_openai::parse_openai_published_catalog(
        include_str!("../data/models.json"),
        "openrouter",
        "openai-completions",
    )
    .map_err(ProviderBuildError::catalog)
}

/// Compatibility name for the leaf-owned OpenRouter catalog.
pub fn openrouter_models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    models()
}

/// Builds a Send OpenRouter registration around a caller-shared family API.
pub fn provider_with_api(
    api: Arc<dyn agentprism_ai::ChatApi>,
    oauth_transport: Arc<dyn agentprism_ai::HttpTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let oauth = Arc::new(OpenRouterOAuth::new(oauth_transport));
    agentprism_provider_common::build_provider(
        "openrouter",
        "OpenRouter",
        models()?,
        agentprism_provider_common::bearer_auth(
            "OpenRouter API key",
            "OPENROUTER_API_KEY",
            Some(oauth),
        ),
        [(ApiId::new("openai-completions"), api)],
    )
}

/// Compatibility name for a caller-shared OpenRouter family API.
pub fn openrouter_provider_with_api(
    api: Arc<dyn agentprism_ai::ChatApi>,
    oauth_transport: Arc<dyn agentprism_ai::HttpTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    provider_with_api(api, oauth_transport)
}

/// Builds the Send OpenRouter registration.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let api = agentprism_openai::openai_completions_api(Arc::clone(&inputs.http));
    provider_with_api(api, inputs.http)
}

/// Builds OpenRouter directly from a raw Send transport.
pub fn openrouter_provider(
    transport: Arc<dyn agentprism_ai::HttpTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    provider(ProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds a local OpenRouter registration around a caller-shared family API.
pub fn local_provider_with_api(
    api: Rc<dyn agentprism_ai::LocalChatApi>,
    oauth_transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let oauth = Rc::new(LocalOpenRouterOAuth::new(oauth_transport));
    agentprism_provider_common::build_local_provider(
        "openrouter",
        "OpenRouter",
        models()?,
        agentprism_provider_common::local_bearer_auth(
            "OpenRouter API key",
            "OPENROUTER_API_KEY",
            Some(oauth),
        ),
        [(ApiId::new("openai-completions"), api)],
    )
}

/// Compatibility name for a caller-shared local OpenRouter family API.
pub fn local_openrouter_provider_with_api(
    api: Rc<dyn agentprism_ai::LocalChatApi>,
    oauth_transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider_with_api(api, oauth_transport)
}

/// Builds the local OpenRouter registration.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let api = agentprism_openai::local_openai_completions_api(Rc::clone(&inputs.http));
    local_provider_with_api(api, inputs.http)
}

/// Builds OpenRouter directly from a raw local transport.
pub fn local_openrouter_provider(
    transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider(LocalProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}
