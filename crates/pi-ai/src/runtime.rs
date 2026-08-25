//! The narrow model-execution capability from Architecture v2 part 1 §3.2
//! and §5, revised by part 2 §2.1 and §9.2.

use crate::{
    AssistantStream, CancellationToken, Context, LocalAssistantStream, LocalBoxFuture, ModelRef,
    SendBoxFuture, SimpleGenerationOptions,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// One provider-neutral request at the [`ModelRuntime`] seam.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Provider/model to execute.
    pub model: ModelRef,
    /// Canonical provider-neutral context.
    pub context: Context,
    /// Common generation options and at most one erased API patch.
    pub options: SimpleGenerationOptions,
}

/// Classification of a failure before an assistant stream is established.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestStartErrorKind {
    /// The canonical request is malformed.
    InvalidRequest,
    /// The provider identifier is not registered.
    UnknownProvider,
    /// The model reference is not registered.
    UnknownModel,
    /// The selected provider/API does not implement the requested operation.
    UnsupportedOperation,
    /// The runtime has no capacity or scripted response available.
    RuntimeUnavailable,
    /// The runtime violated an internal setup invariant.
    Internal,
    /// The request was cancelled before a stream could be established.
    Cancelled,
}

/// Sanitized error returned only before a model stream can be established.
///
/// Once a stream exists, operational failure and cancellation are represented
/// by [`crate::AssistantEvent::Failed`] and
/// [`crate::AssistantEvent::Cancelled`] terminal records instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestStartError {
    /// Stable error classification.
    pub kind: RequestStartErrorKind,
    /// Sanitized diagnostic message.
    pub message: String,
    /// Provider involved in the failed lookup or setup, when known.
    pub provider: Option<crate::ProviderId>,
    /// Full model reference involved in the failed lookup or setup, when known.
    pub model: Option<ModelRef>,
}

impl RequestStartError {
    /// Creates a sanitized request-start error without provider context.
    pub fn new(kind: RequestStartErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider: None,
            model: None,
        }
    }

    /// Adds a model reference and its provider to this error.
    pub fn with_model(mut self, model: ModelRef) -> Self {
        self.provider = Some(model.provider.clone());
        self.model = Some(model);
        self
    }
}

impl fmt::Display for RequestStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RequestStartError {}

/// Thread-safe, object-safe model execution capability consumed by agent core.
pub trait ModelRuntime: Send + Sync + 'static {
    /// Establishes an owned, replay-aware assistant stream.
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, RequestStartError>>;
}

/// Single-threaded, object-safe model execution capability.
///
/// Neither implementors nor returned futures/streams need to be `Send`, so
/// browser WASM and `Rc`-based host integrations can implement this family.
pub trait LocalModelRuntime: 'static {
    /// Establishes an owned local assistant stream.
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>>;
}
