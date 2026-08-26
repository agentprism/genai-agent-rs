//! DeepSeek provider leaf backed by the shared OpenAI Completions family.

#![deny(missing_docs)]

use agentprism_ai::ApiId;
use std::rc::Rc;
use std::sync::Arc;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

/// Returns the pinned DeepSeek catalog owned by this leaf.
pub fn models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    agentprism_openai::parse_openai_published_catalog(
        include_str!("../data/models.json"),
        "deepseek",
        "openai-completions",
    )
    .map_err(ProviderBuildError::catalog)
}

/// Compatibility name for the leaf-owned DeepSeek catalog.
pub fn deepseek_models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    models()
}

/// Builds a Send DeepSeek registration around a caller-shared family API.
pub fn provider_with_api(
    api: Arc<dyn agentprism_ai::ChatApi>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    agentprism_provider_common::build_provider(
        "deepseek",
        "DeepSeek",
        models()?,
        agentprism_provider_common::bearer_auth("DeepSeek API key", "DEEPSEEK_API_KEY", None),
        [(ApiId::new("openai-completions"), api)],
    )
}

/// Compatibility name for a caller-shared DeepSeek family API.
pub fn deepseek_provider_with_api(
    api: Arc<dyn agentprism_ai::ChatApi>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    provider_with_api(api)
}

/// Builds the Send DeepSeek registration.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    provider_with_api(agentprism_openai::openai_completions_api(inputs.http))
}

/// Builds DeepSeek directly from a raw Send transport.
pub fn deepseek_provider(
    transport: Arc<dyn agentprism_ai::HttpTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    provider(ProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds a local DeepSeek registration around a caller-shared family API.
pub fn local_provider_with_api(
    api: Rc<dyn agentprism_ai::LocalChatApi>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    agentprism_provider_common::build_local_provider(
        "deepseek",
        "DeepSeek",
        models()?,
        agentprism_provider_common::local_bearer_auth("DeepSeek API key", "DEEPSEEK_API_KEY", None),
        [(ApiId::new("openai-completions"), api)],
    )
}

/// Compatibility name for a caller-shared local DeepSeek family API.
pub fn local_deepseek_provider_with_api(
    api: Rc<dyn agentprism_ai::LocalChatApi>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider_with_api(api)
}

/// Builds the local DeepSeek registration.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider_with_api(agentprism_openai::local_openai_completions_api(inputs.http))
}

/// Builds DeepSeek directly from a raw local transport.
pub fn local_deepseek_provider(
    transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider(LocalProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}
