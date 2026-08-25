//! Durable deferred-response data and optional execution capabilities from
//! pinned Pi's `types.ts`, `models.ts`, and `api/lazy.ts` contracts.

use crate::{
    ApiId, ApiRequestOptions, AssistantMessage, CancellationToken, LocalBoxFuture, ModelId,
    ModelRef, ProviderId, RequestStartError, SendBoxFuture, Timestamp,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Current persisted deferred-handle schema.
pub const DEFERRED_HANDLE_SCHEMA_VERSION: u32 = 1;

/// Plain durable provider token used to resume one deferred response.
///
/// The value contains no client, future, stream, or process-local state, so it
/// can be persisted by a session and redeemed after a process restart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeferredHandle {
    /// Persisted handle schema version.
    pub schema_version: u32,
    /// Provider that accepted the original request.
    pub provider: ProviderId,
    /// Model to which the original request was submitted.
    pub model_id: ModelId,
    /// API family that owns the provider token.
    pub api: ApiId,
    /// Provider token, such as a response identifier or batch/row identity.
    pub id: String,
    /// Provider expiry time in Unix milliseconds, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// Provider-recommended delay before the next status check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    /// Provider conversion data needed to reconstruct the final assistant
    /// message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl DeferredHandle {
    /// Creates a version-one handle without optional provider metadata.
    pub fn new(
        provider: impl Into<ProviderId>,
        model_id: impl Into<ModelId>,
        api: impl Into<ApiId>,
        id: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: DEFERRED_HANDLE_SCHEMA_VERSION,
            provider: provider.into(),
            model_id: model_id.into(),
            api: api.into(),
            id: id.into(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        }
    }

    /// Returns the provider/model identity used by Models routing.
    pub fn model_ref(&self) -> ModelRef {
        ModelRef {
            provider: self.provider.clone(),
            model: self.model_id.clone(),
        }
    }
}

/// Provider-supported deferred execution capabilities.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeferredCapabilities {
    /// Whether the API can poll or long-poll a deferred response.
    pub fetch: bool,
    /// Whether the API can attempt best-effort cancellation.
    pub cancel: bool,
}

impl DeferredCapabilities {
    /// No deferred operations are supported.
    pub const NONE: Self = Self {
        fetch: false,
        cancel: false,
    };

    /// Deferred fetch is supported, but cancellation is not.
    pub const FETCH: Self = Self {
        fetch: true,
        cancel: false,
    };

    /// Both deferred fetch and cancellation are supported.
    pub const FETCH_AND_CANCEL: Self = Self {
        fetch: true,
        cancel: true,
    };
}

/// Provider window requested for asynchronous continuation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DeferredWindow {
    /// Fifteen-minute provider window.
    #[serde(rename = "15m")]
    FifteenMinutes,
    /// One-hour provider window.
    #[serde(rename = "1h")]
    OneHour,
    /// Twenty-four-hour provider window.
    #[serde(rename = "24h")]
    TwentyFourHours,
}

/// Pi-compatible provider-neutral deferred submission preference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeferredSubmission {
    /// Explicitly disable deferred execution.
    Disabled,
    /// Enable deferred execution with provider defaults.
    Enabled,
    /// Enable deferred execution with an optional provider window.
    Window {
        /// Requested provider window. `None` preserves Pi's `{}` form.
        window: Option<DeferredWindow>,
    },
}

impl DeferredSubmission {
    /// Returns whether the request asks the provider to defer execution.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum EncodedDeferredSubmission {
    Boolean(bool),
    Options {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<DeferredWindow>,
    },
}

impl Serialize for DeferredSubmission {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Disabled => EncodedDeferredSubmission::Boolean(false).serialize(serializer),
            Self::Enabled => EncodedDeferredSubmission::Boolean(true).serialize(serializer),
            Self::Window { window } => {
                EncodedDeferredSubmission::Options { window: *window }.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for DeferredSubmission {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(
            match EncodedDeferredSubmission::deserialize(deserializer)? {
                EncodedDeferredSubmission::Boolean(false) => Self::Disabled,
                EncodedDeferredSubmission::Boolean(true) => Self::Enabled,
                EncodedDeferredSubmission::Options { window } => Self::Window { window },
            },
        )
    }
}

/// Request controls for one deferred fetch/status check.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeferredFetchOptions {
    /// Maximum provider long-poll duration in milliseconds. `None` and zero
    /// both request one status check unless the API family documents more.
    pub wait_ms: Option<u64>,
    /// Common auth-independent transport controls.
    #[serde(flatten)]
    pub request: ApiRequestOptions,
}

/// Request controls for best-effort deferred cancellation.
pub type DeferredCancelOptions = ApiRequestOptions;

/// Optional thread-safe deferred-response execution capability.
///
/// This remains separate from [`crate::ModelRuntime`] so the agent's ordinary
/// model-execution seam stays one-method and consumers request suspension
/// support only where they need it.
pub trait DeferredModelRuntime: crate::ModelRuntime {
    /// Polls or long-polls a deferred response to one terminal assistant
    /// message. A still-pending provider result has finish reason `Deferred`
    /// and carries the same durable handle.
    fn fetch_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredFetchOptions,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantMessage, RequestStartError>>;

    /// Attempts best-effort provider cancellation.
    fn cancel_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredCancelOptions,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), RequestStartError>>;
}

/// Optional single-threaded deferred-response execution capability.
pub trait LocalDeferredModelRuntime: crate::LocalModelRuntime {
    /// Local counterpart to [`DeferredModelRuntime::fetch_deferred`].
    fn fetch_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredFetchOptions,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AssistantMessage, RequestStartError>>;

    /// Local counterpart to [`DeferredModelRuntime::cancel_deferred`].
    fn cancel_deferred(
        &self,
        model: ModelRef,
        handle: DeferredHandle,
        options: DeferredCancelOptions,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), RequestStartError>>;
}
