//! OpenRouter provider leaf backed by the shared OpenAI Completions family.

#![deny(missing_docs)]

mod oauth;

use pi_ai::ApiId;
use std::rc::Rc;
use std::sync::Arc;

pub use oauth::{LocalOpenRouterOAuth, OpenRouterOAuth};
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

/// Returns the pinned OpenRouter catalog owned by this leaf.
pub fn models() -> Result<Vec<pi_ai::ModelDescriptor>, ProviderBuildError> {
    pi_ai_openai::parse_openai_published_catalog(
        include_str!("../data/models.json"),
        "openrouter",
        "openai-completions",
    )
    .map_err(ProviderBuildError::catalog)
}

/// Compatibility name for the leaf-owned OpenRouter catalog.
pub fn openrouter_models() -> Result<Vec<pi_ai::ModelDescriptor>, ProviderBuildError> {
    models()
}

/// Builds a Send OpenRouter registration around a caller-shared family API.
pub fn provider_with_api(
    api: Arc<dyn pi_ai::ChatApi>,
    oauth_transport: Arc<dyn pi_ai::HttpTransport>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    let oauth = Arc::new(OpenRouterOAuth::new(oauth_transport));
    pi_ai_provider_common::build_provider(
        "openrouter",
        "OpenRouter",
        models()?,
        pi_ai_provider_common::bearer_auth("OpenRouter API key", "OPENROUTER_API_KEY", Some(oauth)),
        [(ApiId::new("openai-completions"), api)],
    )
}

/// Compatibility name for a caller-shared OpenRouter family API.
pub fn openrouter_provider_with_api(
    api: Arc<dyn pi_ai::ChatApi>,
    oauth_transport: Arc<dyn pi_ai::HttpTransport>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    provider_with_api(api, oauth_transport)
}

/// Builds the Send OpenRouter registration.
pub fn provider(inputs: ProviderInputs) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    let api = pi_ai_openai::openai_completions_api(Arc::clone(&inputs.http));
    provider_with_api(api, inputs.http)
}

/// Builds OpenRouter directly from a raw Send transport.
pub fn openrouter_provider(
    transport: Arc<dyn pi_ai::HttpTransport>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    provider(ProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds a local OpenRouter registration around a caller-shared family API.
pub fn local_provider_with_api(
    api: Rc<dyn pi_ai::LocalChatApi>,
    oauth_transport: Rc<dyn pi_ai::LocalHttpTransport>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    let oauth = Rc::new(LocalOpenRouterOAuth::new(oauth_transport));
    pi_ai_provider_common::build_local_provider(
        "openrouter",
        "OpenRouter",
        models()?,
        pi_ai_provider_common::local_bearer_auth(
            "OpenRouter API key",
            "OPENROUTER_API_KEY",
            Some(oauth),
        ),
        [(ApiId::new("openai-completions"), api)],
    )
}

/// Compatibility name for a caller-shared local OpenRouter family API.
pub fn local_openrouter_provider_with_api(
    api: Rc<dyn pi_ai::LocalChatApi>,
    oauth_transport: Rc<dyn pi_ai::LocalHttpTransport>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider_with_api(api, oauth_transport)
}

/// Builds the local OpenRouter registration.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    let api = pi_ai_openai::local_openai_completions_api(Rc::clone(&inputs.http));
    local_provider_with_api(api, inputs.http)
}

/// Builds OpenRouter directly from a raw local transport.
pub fn local_openrouter_provider(
    transport: Rc<dyn pi_ai::LocalHttpTransport>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider(LocalProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}
