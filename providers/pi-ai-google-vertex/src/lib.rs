//! Google Vertex AI provider leaf backed by the shared Google family.

#![deny(missing_docs)]

mod auth;

use pi_ai::{ApiFamily, ApiModelConfig, GoogleVertex, ProviderId};
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

pub use auth::*;
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

const VERTEX_MODEL_IDS: &[&str] = &[
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
    "gemini-2.5-pro",
    "gemini-3-flash-preview",
    "gemini-3.1-flash-lite",
    "gemini-3.1-pro-preview",
    "gemini-3.1-pro-preview-customtools",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.6-flash",
    "gemini-3.7-flash",
    "gemini-flash-latest",
    "gemini-flash-lite-latest",
];

/// Returns the pinned Vertex catalog owned by this leaf.
pub fn models() -> Result<Vec<pi_ai::ModelDescriptor>, ProviderBuildError> {
    let mut google = pi_ai_google::google_models().map_err(ProviderBuildError::catalog)?;
    let base_url = Url::parse("https://us-central1-aiplatform.googleapis.com")
        .map_err(ProviderBuildError::configuration)?;
    VERTEX_MODEL_IDS
        .iter()
        .map(|id| {
            let index = google
                .iter()
                .position(|model| model.common.model_ref.model.as_str() == *id)
                .ok_or_else(|| {
                    ProviderBuildError::configuration(format!(
                        "Google catalog omitted Vertex model {id}"
                    ))
                })?;
            let mut model = google.remove(index);
            let ApiModelConfig::GoogleGenerativeAi(config) = model.api else {
                return Err(ProviderBuildError::configuration(format!(
                    "Google model {id} did not use the shared Google family"
                )));
            };
            model.common.model_ref.provider = ProviderId::new("google-vertex");
            model.common.base_url = base_url.clone();
            model.api = ApiModelConfig::GoogleVertex(config);
            Ok(model)
        })
        .collect()
}

/// Compatibility name for the leaf-owned Vertex catalog.
pub fn google_vertex_models() -> Result<Vec<pi_ai::ModelDescriptor>, ProviderBuildError> {
    models()
}

/// Builds the Send Google Vertex AI provider.
pub fn provider(inputs: ProviderInputs) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    build_provider(
        inputs.http.clone(),
        google_vertex_auth_resolver(inputs.http),
    )
}

/// Builds Google Vertex directly from a raw Send transport.
pub fn google_vertex_provider(
    transport: Arc<dyn pi_ai::HttpTransport>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    provider(ProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds the Send provider with a host ADC adapter.
pub fn provider_with_adc_adapter(
    inputs: ProviderInputs,
    adapter: Arc<dyn VertexAdcCredentialAdapter>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    build_provider(
        inputs.http.clone(),
        google_vertex_auth_resolver_with_adc_adapter(inputs.http, adapter),
    )
}

/// Builds Google Vertex directly with a host ADC adapter.
pub fn google_vertex_provider_with_adc_adapter(
    transport: Arc<dyn pi_ai::HttpTransport>,
    adapter: Arc<dyn VertexAdcCredentialAdapter>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    provider_with_adc_adapter(
        ProviderInputs {
            http: transport,
            environment: Default::default(),
        },
        adapter,
    )
}

fn build_provider(
    transport: Arc<dyn pi_ai::HttpTransport>,
    auth: Arc<dyn pi_ai::AuthResolver>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    pi_ai::ProviderRegistration::builder("google-vertex")
        .display_name("Google Vertex AI")
        .headers(pi_ai_google::google_default_headers())
        .auth(auth)
        .models(models()?)
        .api(
            GoogleVertex::API_ID,
            pi_ai_google::google_vertex_api(transport),
        )
        .build()
        .map_err(ProviderBuildError::Registration)
}

/// Builds the local Google Vertex AI provider.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    build_local_provider(
        inputs.http.clone(),
        local_google_vertex_auth_resolver(inputs.http),
    )
}

/// Builds Google Vertex directly from a raw local transport.
pub fn local_google_vertex_provider(
    transport: Rc<dyn pi_ai::LocalHttpTransport>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider(LocalProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds the local provider with a host ADC adapter.
pub fn local_provider_with_adc_adapter(
    inputs: LocalProviderInputs,
    adapter: Rc<dyn LocalVertexAdcCredentialAdapter>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    build_local_provider(
        inputs.http.clone(),
        local_google_vertex_auth_resolver_with_adc_adapter(inputs.http, adapter),
    )
}

/// Builds local Google Vertex directly with a host ADC adapter.
pub fn local_google_vertex_provider_with_adc_adapter(
    transport: Rc<dyn pi_ai::LocalHttpTransport>,
    adapter: Rc<dyn LocalVertexAdcCredentialAdapter>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider_with_adc_adapter(
        LocalProviderInputs {
            http: transport,
            environment: Default::default(),
        },
        adapter,
    )
}

fn build_local_provider(
    transport: Rc<dyn pi_ai::LocalHttpTransport>,
    auth: Rc<dyn pi_ai::LocalAuthResolver>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    pi_ai::LocalProviderRegistration::builder("google-vertex")
        .display_name("Google Vertex AI")
        .headers(pi_ai_google::google_default_headers())
        .auth(auth)
        .models(models()?)
        .api(
            GoogleVertex::API_ID,
            pi_ai_google::local_google_vertex_api(transport),
        )
        .build()
        .map_err(ProviderBuildError::Registration)
}
