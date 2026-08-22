//! Provider-neutral simple generation and lowering inputs from Architecture v2
//! part 1 §3.4 as revised by part 2 §3.3–§3.7.

use crate::{
    ApiId, CommonModelDescriptor, Context, ExtensionMap, HeaderMapSpec, OrderedJsonObject,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use std::fmt;
use url::Url;

/// Provider-neutral reasoning level accepted by simple generation calls.
///
/// Reasoning is disabled by leaving [`SimpleGenerationOptions::reasoning`]
/// unset, matching pinned Pi's `SimpleStreamOptions` contract.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    /// Smallest supported reasoning effort.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extended high reasoning effort.
    Xhigh,
    /// Provider maximum reasoning effort.
    Max,
}

impl ReasoningLevel {
    /// Resolves extended levels for an API/model without native extended-level
    /// support according to the caller's fallback policy.
    pub fn resolve_extended(
        self,
        native_xhigh: bool,
        native_max: bool,
        fallback: ReasoningFallback,
    ) -> Result<Self, LoweringError> {
        let supported = match self {
            Self::Xhigh => native_xhigh,
            Self::Max => native_max,
            Self::Minimal | Self::Low | Self::Medium | Self::High => true,
        };
        if supported {
            return Ok(self);
        }
        match fallback {
            ReasoningFallback::Strict => {
                Err(LoweringError::UnsupportedReasoningLevel { requested: self })
            }
            ReasoningFallback::Clamp => Ok(Self::High),
        }
    }
}

/// Policy for reasoning levels a target API/model does not support
/// (Architecture v2 part 2 §3.7).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningFallback {
    /// Reject an explicitly unsupported level.
    Strict,
    /// Clamp to the highest supported lower level, matching Pi parity mode.
    #[default]
    Clamp,
}

/// Per-level token budgets used by token-based reasoning APIs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThinkingBudgets {
    /// Minimal reasoning budget.
    pub minimal: Option<u32>,
    /// Low reasoning budget.
    pub low: Option<u32>,
    /// Medium reasoning budget.
    pub medium: Option<u32>,
    /// High reasoning budget.
    pub high: Option<u32>,
}

impl Default for ThinkingBudgets {
    fn default() -> Self {
        Self {
            minimal: Some(1_024),
            low: Some(2_048),
            medium: Some(8_192),
            high: Some(16_384),
        }
    }
}

impl ThinkingBudgets {
    /// Returns the configured or Pi-default budget for a reasoning level.
    /// Extended levels use the high token budget on budget-based APIs.
    pub fn budget_for(&self, level: ReasoningLevel) -> Option<u32> {
        match level {
            ReasoningLevel::Minimal => self.minimal,
            ReasoningLevel::Low => self.low,
            ReasoningLevel::Medium => self.medium,
            ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => self.high,
        }
    }
}

/// Prompt-cache retention preference shared by simple calls.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    /// Disable provider prompt caching.
    None,
    /// Use ordinary short-lived caching. This is Pi's default.
    #[default]
    Short,
    /// Request the provider's long-retention mode when supported.
    Long,
}

/// Provider-neutral simple tool-selection behavior.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Let the model decide whether to call a tool.
    #[default]
    Auto,
    /// Prevent tool calls.
    None,
}

/// Fully merged common sampling plan passed into API-family lowering.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SamplingPlan {
    /// Requested temperature.
    pub temperature: Option<f32>,
    /// Requested nucleus-sampling probability.
    pub top_p: Option<f32>,
    /// Requested deterministic seed.
    pub seed: Option<u64>,
    /// Insertion-ordered additional API-family sampling parameters.
    pub additional: OrderedJsonObject,
}

/// One erased API-family options patch used by dynamic/FFI callers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErasedApiOptionsPatch {
    /// API family that owns the patch schema.
    pub api: ApiId,
    /// Version of the API-family patch schema.
    pub schema_version: u32,
    /// Exact JSON patch value.
    pub value: Box<RawValue>,
}

impl PartialEq for ErasedApiOptionsPatch {
    fn eq(&self, other: &Self) -> bool {
        self.api == other.api
            && self.schema_version == other.schema_version
            && self.value.get() == other.value.get()
    }
}

/// One model descriptor narrowed to a concrete API family.
///
/// This is an owned, typed view of [`crate::ModelDescriptor`]. API registry
/// adapters perform the checked conversion from the erased enum before calling
/// family-specific lowering.
pub struct TypedModelDescriptor<A: ApiFamily + ?Sized> {
    /// API-independent catalog fields.
    pub common: CommonModelDescriptor,
    /// API-family-specific lowering configuration.
    pub config: A::ModelConfig,
    /// Namespaced provider extensions preserved alongside the typed config.
    pub extensions: ExtensionMap,
}

impl<A: ApiFamily + ?Sized> Clone for TypedModelDescriptor<A> {
    fn clone(&self) -> Self {
        Self {
            common: self.common.clone(),
            config: self.config.clone(),
            extensions: self.extensions.clone(),
        }
    }
}

/// Borrowed inputs available while planning provider-neutral simple options.
pub struct SimpleLoweringContext<'a, A: ApiFamily + ?Sized> {
    /// Target model narrowed to this API family.
    pub model: &'a TypedModelDescriptor<A>,
    /// Compatibility resolved from the effective endpoint and model overrides.
    pub compat: &'a A::Compat,
    /// Endpoint after provider authentication and base-URL resolution.
    pub effective_base_url: &'a Url,
    /// Pi-equivalent estimate for the canonical input context.
    pub estimated_input_tokens: u64,
    /// Context tokens remaining after the safety reservation.
    pub available_context_tokens: u64,
}

/// Borrowed inputs available while encoding a family-specific wire request.
///
/// Part 2 §3.2 leaves this context's fields open. The M1 contract carries the
/// typed model, resolved compatibility, effective endpoint, and canonical
/// context by reference so provider crates do not need to recover them from an
/// erased registry payload.
pub struct EncodeContext<'a, A: ApiFamily + ?Sized> {
    /// Target model narrowed to this API family.
    pub model: &'a TypedModelDescriptor<A>,
    /// Canonical context after handoff projection.
    pub context: &'a Context,
    /// Compatibility resolved for the effective endpoint.
    pub compat: &'a A::Compat,
    /// Endpoint after provider authentication and base-URL resolution.
    pub effective_base_url: &'a Url,
}

/// Typed API-family lowering and wire-encoding contract
/// (Architecture v2 part 2 §3.2).
pub trait ApiFamily: Send + Sync + 'static {
    /// Open API-family identifier.
    const API_ID: &'static str;

    /// Endpoint-detected and model-overridden compatibility settings.
    type Compat: Clone + Send + Sync + Serialize + DeserializeOwned;
    /// Typed API-family catalog configuration.
    type ModelConfig: Clone + Send + Sync;
    /// Fully planned API-family generation options.
    type FullOptions: Clone + Send + Sync;
    /// Typed patch applied after common simple options.
    type OptionsPatch: Clone + Send + Sync + Default;
    /// Provider wire request before transport middleware.
    type WireRequest: Send + Sync;

    /// Resolves endpoint-detected defaults with typed model overrides.
    fn resolve_compat(
        effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError>;

    /// Plans common simple options into this API family's full option type.
    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError>;

    /// Encodes a projected canonical context into the provider wire type.
    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError>;
}

/// Alternate typed and erased representations of one API-family patch
/// (Architecture v2 part 2 §3.3).
#[derive(Clone, Debug, PartialEq)]
pub enum ApiOptionsInput<A: ApiFamily> {
    /// No API-specific patch.
    None,
    /// Statically typed Rust patch.
    Typed(A::OptionsPatch),
    /// Versioned erased patch from configuration or FFI.
    Erased(ErasedApiOptionsPatch),
}

impl<A: ApiFamily> ApiOptionsInput<A> {
    /// Resolves two builder sources and rejects the ambiguous mixed case.
    pub fn from_sources(
        typed: Option<A::OptionsPatch>,
        erased: Option<ErasedApiOptionsPatch>,
    ) -> Result<Self, LoweringError> {
        let expected = ApiId::new(A::API_ID);
        match (typed, erased) {
            (Some(_), Some(_)) => Err(LoweringError::ConflictingApiOptions { api: expected }),
            (Some(patch), None) => Ok(Self::Typed(patch)),
            (None, Some(patch)) if patch.api == expected => Ok(Self::Erased(patch)),
            (None, Some(patch)) => Err(LoweringError::UnknownApiOptions {
                expected,
                actual: patch.api,
            }),
            (None, None) => Ok(Self::None),
        }
    }
}

/// Stable provider-neutral simple-generation options.
///
/// Part 2 §3.3 replaces Part 1's multi-entry `extensions[api]` bag with the
/// single versioned `api_options` patch below. Typed callers carry the same one
/// patch as [`ApiOptionsInput::Typed`] until API-family lowering.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SimpleGenerationOptions {
    /// Requested maximum number of output tokens.
    pub max_output_tokens: Option<u32>,
    /// Requested sampling temperature.
    pub temperature: Option<f32>,
    /// Requested nucleus-sampling probability.
    pub top_p: Option<f32>,
    /// Stop strings in caller order.
    pub stop: Vec<String>,
    /// Requested reasoning effort; `None` disables simple reasoning.
    pub reasoning: Option<ReasoningLevel>,
    /// Strict-versus-clamp behavior for unsupported reasoning levels.
    pub reasoning_fallback: ReasoningFallback,
    /// Optional per-level token-budget overrides.
    pub thinking_budgets: ThinkingBudgets,
    /// Optional deterministic seed.
    pub seed: Option<u64>,
    /// Optional provider session-affinity identifier.
    pub session_id: Option<String>,
    /// Logical request headers, including explicit deletion markers.
    pub headers: HeaderMapSpec,
    /// Prompt-cache retention preference; defaults are applied during planning.
    pub cache_retention: Option<CacheRetention>,
    /// Provider-neutral tool selection; defaults are applied during planning.
    pub tool_choice: Option<ToolChoice>,
    /// The sole erased API-family patch for dynamic callers.
    pub api_options: Option<ErasedApiOptionsPatch>,
}

/// Failure while lowering provider-neutral options into an API family.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoweringError {
    /// Both typed and erased forms of the same patch were supplied.
    ConflictingApiOptions {
        /// API family whose patch was ambiguous.
        api: ApiId,
    },
    /// An erased patch names a different API family from the target.
    UnknownApiOptions {
        /// Target API family.
        expected: ApiId,
        /// API family named by the patch.
        actual: ApiId,
    },
    /// The target explicitly does not support the requested reasoning level.
    UnsupportedReasoningLevel {
        /// Level the caller requested.
        requested: ReasoningLevel,
    },
    /// API-family compatibility or option data is invalid.
    InvalidConfiguration {
        /// Sanitized validation detail.
        message: String,
    },
}

impl fmt::Display for LoweringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConflictingApiOptions { api } => {
                write!(
                    formatter,
                    "typed and erased API options both supplied for {api}"
                )
            }
            Self::UnknownApiOptions { expected, actual } => write!(
                formatter,
                "API options for {actual} cannot be applied to target API {expected}"
            ),
            Self::UnsupportedReasoningLevel { requested } => {
                write!(formatter, "reasoning level {requested:?} is unsupported")
            }
            Self::InvalidConfiguration { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for LoweringError {}

/// Failure while encoding fully lowered options into a provider wire request.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncodeError {
    /// A redacted reasoning block has no complete applicable replay payload.
    MissingRedactedThinkingPayload,
    /// The projected canonical context cannot be represented by this API.
    InvalidRequest {
        /// Sanitized encoding detail.
        message: String,
    },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRedactedThinkingPayload => {
                formatter.write_str("redacted thinking is missing its replay payload")
            }
            Self::InvalidRequest { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for EncodeError {}
