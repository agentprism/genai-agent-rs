//! Qwen Token Plan Individual provider leaf crate.
#![deny(missing_docs)]
use std::collections::BTreeSet;
use std::fmt;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

/// Exact pinned Individual model allowlist used by strict catalog generation.
pub const STRICT_MODEL_IDS: &[&str] = &[
    "deepseek-v4-flash-0731",
    "deepseek-v4-pro",
    "deepseek-v4-pro-0813",
    "glm-5.2",
    "qwen3.6-flash",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.8-max",
];

/// Upstream capability observation consumed before catalog publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StrictSourceModel<'a> {
    /// Upstream model identity.
    pub id: &'a str,
    /// Whether upstream still advertises tool calling.
    pub supports_tools: bool,
}

/// Exact-ID mismatch found during strict catalog validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StrictCatalogError {
    missing: Vec<String>,
    extra: Vec<String>,
}

impl StrictCatalogError {
    /// Missing pinned identities in sorted order.
    pub fn missing(&self) -> &[String] {
        &self.missing
    }

    /// Unexpected generated identities in sorted order.
    pub fn extra(&self) -> &[String] {
        &self.extra
    }
}

impl fmt::Display for StrictCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut differences = Vec::new();
        if !self.missing.is_empty() {
            differences.push(format!("missing: {}", self.missing.join(", ")));
        }
        if !self.extra.is_empty() {
            differences.push(format!("extra: {}", self.extra.join(", ")));
        }
        write!(
            formatter,
            "qwen-token-plan-individual model IDs do not match ({})",
            differences.join("; ")
        )
    }
}

impl std::error::Error for StrictCatalogError {}

/// Filters source models by tool support and validates the complete result
/// before returning any publishable identities.
pub fn validate_strict_source_models<'a>(
    source: impl IntoIterator<Item = StrictSourceModel<'a>>,
) -> Result<Vec<&'a str>, StrictCatalogError> {
    let generated = source
        .into_iter()
        .filter(|model| model.supports_tools)
        .map(|model| model.id)
        .collect::<BTreeSet<_>>();
    let expected = STRICT_MODEL_IDS.iter().copied().collect::<BTreeSet<_>>();
    let missing = expected
        .difference(&generated)
        .map(|id| (*id).to_owned())
        .collect::<Vec<_>>();
    let extra = generated
        .difference(&expected)
        .map(|id| (*id).to_owned())
        .collect::<Vec<_>>();
    if missing.is_empty() && extra.is_empty() {
        Ok(generated.into_iter().collect())
    } else {
        Err(StrictCatalogError { missing, extra })
    }
}

agentprism_provider_common::define_static_provider! {
    id: "qwen-token-plan-individual", name: "Qwen Token Plan Individual", auth_name: "Qwen Token Plan Individual API key", env: "QWEN_TOKEN_PLAN_API_KEY",
    catalog: agentprism_openai::parse_openai_published_catalog(include_str!("../data/models.json"), "qwen-token-plan-individual", "openai-completions"),
    send_apis: [("openai-completions", agentprism_openai::openai_completions_api(inputs.http.clone()))],
    local_apis: [("openai-completions", agentprism_openai::local_openai_completions_api(inputs.http.clone()))]
}
