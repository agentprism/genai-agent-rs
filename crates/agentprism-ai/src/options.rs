//! Provider-neutral simple generation and lowering inputs from Architecture v2
//! part 1 §3.4 as revised by part 2 §3.3–§3.7.

use crate::{
    ApiId, ApiModelConfig, CommonModelDescriptor, Context, ExtensionMap, HeaderMapSpec,
    ModelDescriptor, OrderedJsonObject, TokenEstimator,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use std::{any::Any, fmt, sync::Arc};
use url::Url;

/// Provider-neutral reasoning level accepted by simple generation calls.
///
/// Reasoning is disabled by leaving [`SimpleGenerationOptions::reasoning`]
/// unset, matching pinned Pi's `SimpleStreamOptions` contract.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    /// Disable model reasoning. Agent state uses this explicit value while
    /// simple request options may continue to use `None` for an unspecified
    /// request-level preference.
    Off,
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
            Self::Off | Self::Minimal | Self::Low | Self::Medium | Self::High => true,
        };
        if supported {
            return Ok(self);
        }
        match fallback {
            ReasoningFallback::Strict => {
                Err(LoweringError::UnsupportedReasoningLevel { requested: self })
            }
            ReasoningFallback::Clamp => match self {
                // Pinned Pi searches upward from the requested position before
                // searching downward, so an xhigh hole clamps to native max.
                Self::Xhigh if native_max => Ok(Self::Max),
                Self::Max if native_xhigh => Ok(Self::Xhigh),
                Self::Xhigh | Self::Max => Ok(Self::High),
                Self::Off | Self::Minimal | Self::Low | Self::Medium | Self::High => {
                    unreachable!("ordinary reasoning levels are always supported")
                }
            },
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
    /// Search supported levels upward first and then downward, matching Pi.
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
        let defaults = Self::default();
        match level {
            ReasoningLevel::Off => None,
            ReasoningLevel::Minimal => self.minimal.or(defaults.minimal),
            ReasoningLevel::Low => self.low.or(defaults.low),
            ReasoningLevel::Medium => self.medium.or(defaults.medium),
            ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => {
                self.high.or(defaults.high)
            }
        }
    }
}

/// Tokens reserved for provider context growth before output planning.
pub const CONTEXT_SAFETY_TOKENS: u64 = 4_096;

/// Tokens always retained for an answer when thinking shares its ceiling.
pub const MIN_ANSWER_TOKENS: u32 = 1_024;

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
    /// Named simple-request temperature, before the later sampling overlay.
    pub temperature: Option<f32>,
    /// Named simple-request nucleus probability, before the later overlay.
    pub top_p: Option<f32>,
    /// Named simple-request deterministic seed, before the later overlay.
    pub seed: Option<u64>,
    /// Insertion-ordered model/request `samplingParams` overlay. OpenAI-family
    /// encoders apply this object after named fields, so keys here win.
    pub additional: OrderedJsonObject,
}

/// API-independent result of simple-generation planning
/// (Architecture v2 part 2 §3.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommonSimplePlan {
    /// Context- and model-clamped maximum output tokens.
    pub max_output_tokens: u32,
    /// Model defaults overlaid by request sampling values.
    pub sampling: SamplingPlan,
    /// Prompt-cache retention selection.
    pub cache_retention: CacheRetention,
    /// Optional session-affinity value.
    pub session_id: Option<String>,
    /// Provider-neutral tool selection.
    pub tool_choice: ToolChoice,
    /// Requested reasoning level before API-family mapping.
    pub reasoning: Option<ReasoningLevel>,
}

/// Plans the common portion of a simple request before API-family lowering.
pub fn plan_common(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    estimator: &dyn TokenEstimator,
) -> Result<CommonSimplePlan, LoweringError> {
    let requested = simple
        .max_output_tokens
        .unwrap_or(model.common.limits.max_output_tokens);
    let max_output_tokens = if model.common.limits.context_window == 0 {
        requested.max(1)
    } else {
        let estimated = estimator.estimate(context)?;
        let available = model
            .common
            .limits
            .context_window
            .saturating_sub(estimated)
            .saturating_sub(CONTEXT_SAFETY_TOKENS);
        requested.min(u32::try_from(available.max(1)).unwrap_or(u32::MAX))
    };

    Ok(CommonSimplePlan {
        max_output_tokens,
        sampling: merge_sampling(model_sampling_defaults(model), simple),
        cache_retention: simple.cache_retention.unwrap_or_default(),
        session_id: simple.session_id.clone(),
        tool_choice: simple.tool_choice.unwrap_or_default(),
        reasoning: simple.reasoning,
    })
}

/// Pi's reasoning-budget expansion and answer-room reservation.
pub fn plan_thinking_budget(
    explicit_answer_cap: Option<u32>,
    model_max_output_tokens: u32,
    reasoning_level: ReasoningLevel,
    budgets: &ThinkingBudgets,
) -> Result<ThinkingBudgetPlan, LoweringError> {
    let mut thinking_budget =
        budgets
            .budget_for(reasoning_level)
            .ok_or_else(|| LoweringError::InvalidConfiguration {
                message: "reasoning is disabled and has no thinking budget".to_owned(),
            })?;
    let max_output_tokens = explicit_answer_cap.map_or(model_max_output_tokens, |answer| {
        answer
            .saturating_add(thinking_budget)
            .min(model_max_output_tokens)
    });

    thinking_budget = thinking_budget.min(max_output_tokens.saturating_sub(MIN_ANSWER_TOKENS));

    Ok(ThinkingBudgetPlan {
        max_output_tokens,
        thinking_budget,
    })
}

/// Output ceiling and nested thinking allocation from [`plan_thinking_budget`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThinkingBudgetPlan {
    /// Expanded response ceiling, never above the catalog model limit.
    pub max_output_tokens: u32,
    /// Thinking allocation after preserving answer room.
    pub thinking_budget: u32,
}

fn model_sampling_defaults(model: &ModelDescriptor) -> &OrderedJsonObject {
    match &model.api {
        ApiModelConfig::OpenAiCompletions(config) => &config.sampling_defaults,
        ApiModelConfig::OpenAiResponses(config) | ApiModelConfig::OpenAiCodexResponses(config) => {
            &config.sampling_defaults
        }
        ApiModelConfig::AnthropicMessages(_)
        | ApiModelConfig::GoogleGenerativeAi(_)
        | ApiModelConfig::GoogleVertex(_)
        | ApiModelConfig::BedrockConverse(_)
        | ApiModelConfig::MistralConversations(_)
        | ApiModelConfig::Custom(_) => {
            static EMPTY: std::sync::LazyLock<OrderedJsonObject> =
                std::sync::LazyLock::new(OrderedJsonObject::new);
            &EMPTY
        }
    }
}

fn merge_sampling(defaults: &OrderedJsonObject, simple: &SimpleGenerationOptions) -> SamplingPlan {
    let mut additional = defaults.clone();
    for (name, value) in &simple.sampling {
        additional.insert(name.clone(), value.clone());
    }

    SamplingPlan {
        temperature: simple.temperature,
        top_p: simple.top_p,
        seed: simple.seed,
        additional,
    }
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
    type FullOptions: Clone + Send + Sync + 'static;
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

/// Type-erased fully API-specific options carried through provider dispatch.
///
/// Unlike [`ErasedApiOptionsPatch`], this value is an in-process typed Rust
/// object rather than a serialized simple-options patch. The API handler that
/// registered the matching family recovers the exact associated
/// [`ApiFamily::FullOptions`] type without invoking simple lowering.
#[derive(Clone)]
pub struct ErasedApiFullOptions {
    /// API family that owns the full options value.
    pub api: ApiId,
    value: Arc<dyn Any + Send + Sync>,
}

impl ErasedApiFullOptions {
    /// Erases one API family's fully typed options.
    pub fn new<A: ApiFamily>(value: A::FullOptions) -> Self {
        Self {
            api: ApiId::new(A::API_ID),
            value: Arc::new(value),
        }
    }

    /// Recovers the full options when this value belongs to `A`.
    pub fn downcast_ref<A: ApiFamily>(&self) -> Option<&A::FullOptions> {
        (self.api.as_str() == A::API_ID)
            .then(|| self.value.downcast_ref::<A::FullOptions>())
            .flatten()
    }
}

impl fmt::Debug for ErasedApiFullOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ErasedApiFullOptions")
            .field("api", &self.api)
            .field("value", &"<redacted typed options>")
            .finish()
    }
}

/// API-independent transport controls for a fully API-specific request.
///
/// Pi's full provider options extend its common request options. Rust keeps
/// those transport concerns separate from [`ApiFamily::FullOptions`] so the
/// family type contains only lowering and wire-shaping data.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiRequestOptions {
    /// Maximum retry attempts after the initial pre-stream request.
    pub max_retries: Option<u32>,
    /// Maximum provider-requested retry delay in milliseconds.
    pub max_retry_delay_ms: Option<u64>,
    /// HTTP response-establishment timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Preferred provider transport when an API family supports more than
    /// one protocol.
    pub transport: Option<StreamTransport>,
    /// WebSocket connection/open timeout in milliseconds. Stream idleness is
    /// governed separately by `timeout_ms`.
    pub websocket_connect_timeout_ms: Option<u64>,
    /// Optional provider session-affinity identifier.
    pub session_id: Option<String>,
    /// Explicit logical request headers, including deletion markers.
    pub headers: HeaderMapSpec,
}

impl From<&SimpleGenerationOptions> for ApiRequestOptions {
    fn from(options: &SimpleGenerationOptions) -> Self {
        Self {
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            timeout_ms: options.timeout_ms,
            transport: options.transport,
            websocket_connect_timeout_ms: options.websocket_connect_timeout_ms,
            session_id: options.session_id.clone(),
            headers: options.headers.clone(),
        }
    }
}

impl fmt::Debug for ApiRequestOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiRequestOptions")
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("timeout_ms", &self.timeout_ms)
            .field("transport", &self.transport)
            .field(
                "websocket_connect_timeout_ms",
                &self.websocket_connect_timeout_ms,
            )
            .field(
                "session_id",
                &self.session_id.as_ref().map(|_| "<redacted session id>"),
            )
            .field("headers", &"<redacted headers>")
            .finish()
    }
}

/// Common provider transport selection from pinned Pi's `StreamOptions`.
///
/// API families with a single transport ignore this value. OpenAI Codex
/// Responses implements every variant.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamTransport {
    /// Force HTTP Server-Sent Events.
    Sse,
    /// Force a WebSocket request carrying the full canonical context.
    Websocket,
    /// Force WebSocket context continuation when the cached prefix matches.
    WebsocketCached,
    /// Prefer cached WebSocket and fall back to SSE before semantic output.
    #[default]
    Auto,
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
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SimpleGenerationOptions {
    /// Maximum retry attempts after the initial pre-stream request.
    pub max_retries: Option<u32>,
    /// Maximum provider-requested retry delay in milliseconds. Zero disables
    /// the cap.
    pub max_retry_delay_ms: Option<u64>,
    /// HTTP establishment timeout in milliseconds for transports that support
    /// it.
    pub timeout_ms: Option<u64>,
    /// Preferred provider transport for multi-transport API families.
    pub transport: Option<StreamTransport>,
    /// WebSocket connection/open timeout in milliseconds.
    pub websocket_connect_timeout_ms: Option<u64>,
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
    ///
    /// The outer option is semantically significant: Google applies its own
    /// model-specific budget table when this object is omitted, but honors a
    /// caller-supplied object even when every value equals Pi's shared
    /// defaults.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Optional deterministic seed.
    pub seed: Option<u64>,
    /// Insertion-ordered request sampling parameters. These overlay catalog
    /// sampling defaults and are applied after named request fields by API
    /// families that support Pi's `samplingParams` contract.
    pub sampling: OrderedJsonObject,
    /// Optional provider session-affinity identifier.
    pub session_id: Option<String>,
    /// Logical request headers, including explicit deletion markers.
    pub headers: HeaderMapSpec,
    /// Prompt-cache retention preference; defaults are applied during planning.
    pub cache_retention: Option<CacheRetention>,
    /// Provider-neutral tool selection; defaults are applied during planning.
    pub tool_choice: Option<ToolChoice>,
    /// Ask a capable API family to return a durable handle and continue the
    /// request asynchronously.
    pub deferred: Option<crate::DeferredSubmission>,
    /// The sole erased API-family patch for dynamic callers.
    pub api_options: Option<ErasedApiOptionsPatch>,
}

impl fmt::Debug for SimpleGenerationOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SimpleGenerationOptions")
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("timeout_ms", &self.timeout_ms)
            .field("transport", &self.transport)
            .field(
                "websocket_connect_timeout_ms",
                &self.websocket_connect_timeout_ms,
            )
            .field("max_output_tokens", &self.max_output_tokens)
            .field("temperature", &self.temperature)
            .field("top_p", &self.top_p)
            .field("stop", &self.stop)
            .field("reasoning", &self.reasoning)
            .field("reasoning_fallback", &self.reasoning_fallback)
            .field("thinking_budgets", &self.thinking_budgets)
            .field("seed", &self.seed)
            .field("sampling", &self.sampling)
            .field(
                "session_id",
                &self.session_id.as_ref().map(|_| "<redacted session id>"),
            )
            .field("headers", &"<redacted headers>")
            .field("cache_retention", &self.cache_retention)
            .field("tool_choice", &self.tool_choice)
            .field("deferred", &self.deferred)
            .field(
                "api_options",
                &self.api_options.as_ref().map(|_| "<redacted API options>"),
            )
            .finish()
    }
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
