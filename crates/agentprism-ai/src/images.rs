//! Image-generation contracts and provider dispatch capability.
//!
//! Image generation is a separate catalog and execution surface from chat:
//! OpenRouter publishes models that may appear in both catalogs with different
//! capability metadata.  Keeping the catalogs distinct preserves Pi's
//! `ImagesModel` contract without weakening the narrow chat runtime seam.

use crate::{
    ApiId, ApiRequestOptions, AuthResolutionOverrides, CancellationToken, Cost,
    ErasedPayloadTransform, HeaderMapSpec, LocalAttemptMiddleware, LocalBoxFuture,
    LocalErasedPayloadTransform, LocalResponseObserver, LocalRetryClassifier, ModelId,
    ModelPricing, ModelRef, ProviderId, ResponseObserver, RetryClassifier, RetryPolicy,
    SecretString, SendBoxFuture, Timestamp, Usage,
};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

/// OpenRouter Images API-family identifier.
pub const OPENROUTER_IMAGES_API_ID: &str = "openrouter-images";

/// One ordered input or output modality in an image-model descriptor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageModality {
    /// UTF-8 text.
    Text,
    /// Base64-encoded image bytes.
    Image,
}

/// Image-generation model descriptor equivalent to Pi's `ImagesModel`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImageModelDescriptor {
    /// Provider/model lookup identity.
    pub model_ref: ModelRef,
    /// Human-readable model name.
    pub display_name: String,
    /// Image API-family identifier.
    pub api: ApiId,
    /// Default provider endpoint.
    pub base_url: Url,
    /// Accepted modalities in provider-published order.
    pub input: Vec<ImageModality>,
    /// Produced modalities in provider-published order.
    pub output: Vec<ImageModality>,
    /// Exact fixed-point token pricing.
    pub pricing: ModelPricing,
    /// Per-model logical headers.
    #[serde(default)]
    pub headers: HeaderMapSpec,
}

/// One image-generation input or output item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageGenerationContent {
    /// Text content.
    Text {
        /// Text value.
        text: String,
    },
    /// Base64 image content without a data-URL prefix.
    Image {
        /// Base64 image bytes.
        data: String,
        /// Image media type.
        #[serde(rename = "mimeType", alias = "mime_type")]
        mime_type: String,
    },
}

impl ImageGenerationContent {
    /// Creates text content.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Creates base64 image content.
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }
}

/// Canonical image-generation input context.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImageGenerationContext {
    /// Ordered prompt content.
    pub input: Vec<ImageGenerationContent>,
}

/// Image-generation terminal reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageGenerationStopReason {
    /// Provider returned a successful response.
    Stop,
    /// Provider or host failed the request.
    Error,
    /// Caller cancelled the request.
    Aborted,
}

/// Complete non-streaming image-generation result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssistantImages {
    /// API family used for the request.
    pub api: ApiId,
    /// Provider that served the request.
    pub provider: ProviderId,
    /// Requested model.
    pub model: ModelId,
    /// Ordered text and image output.
    pub output: Vec<ImageGenerationContent>,
    /// Provider response identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Provider-reported normalized usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Fixed-point provider-priced cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// Terminal reason.
    pub stop_reason: ImageGenerationStopReason,
    /// Secret-free terminal error message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Creation time in Unix milliseconds.
    pub timestamp: Timestamp,
}

impl AssistantImages {
    /// Creates the initial successful-shaped value used by Pi's decoder.
    pub fn empty(model: &ImageModelDescriptor) -> Self {
        Self {
            api: model.api.clone(),
            provider: model.model_ref.provider.clone(),
            model: model.model_ref.model.clone(),
            output: Vec::new(),
            response_id: None,
            usage: None,
            cost: None,
            stop_reason: ImageGenerationStopReason::Stop,
            error_message: None,
            timestamp: image_timestamp_now(),
        }
    }

    /// Creates Pi's in-band failed or aborted result.
    pub fn failure(
        model_ref: &ModelRef,
        api: impl Into<ApiId>,
        reason: ImageGenerationStopReason,
        message: impl Into<String>,
    ) -> Self {
        Self {
            api: api.into(),
            provider: model_ref.provider.clone(),
            model: model_ref.model.clone(),
            output: Vec::new(),
            response_id: None,
            usage: None,
            cost: None,
            stop_reason: reason,
            error_message: Some(message.into()),
            timestamp: image_timestamp_now(),
        }
    }
}

/// Per-call image-generation options.
#[derive(Clone, Debug, Default)]
pub struct ImageGenerationOptions {
    /// Common HTTP, retry, header, and telemetry controls.
    pub request: ApiRequestOptions,
    /// Explicit request auth and environment overrides.
    pub auth: AuthResolutionOverrides,
    /// Provider-specific request metadata.
    ///
    /// This is the owned Rust equivalent of Pi's
    /// `ImagesOptions.metadata?: Record<string, unknown>`. API families may
    /// consume the keys they understand and ignore the rest.
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Fully resolved image request passed from `Models` to one provider API.
pub struct ResolvedImageRequest {
    /// Selected image model.
    pub model: ImageModelDescriptor,
    /// Canonical image input.
    pub context: ImageGenerationContext,
    /// Common request controls.
    pub request_options: ApiRequestOptions,
    /// Effective endpoint.
    pub endpoint: Url,
    /// Final logical headers.
    pub headers: HeaderMap,
    /// Credential-derived invariant headers.
    pub auth_headers: HeaderMap,
    /// Provider-resolved environment with request values overlaid per key.
    pub environment: BTreeMap<String, String>,
    /// Provider-specific request metadata preserved from the public options.
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// Resolved API key for SDK-shaped adapters.
    pub api_key: Option<SecretString>,
    /// Logical payload transforms in registration order.
    pub payload_transforms: Arc<[Arc<dyn ErasedPayloadTransform>]>,
    /// Resolved retry policy.
    pub retry_policy: RetryPolicy,
    /// Per-attempt establishment timeout.
    pub timeout: Option<Duration>,
    /// Provider retry classifier.
    pub retry_classifier: Arc<dyn RetryClassifier>,
    /// Response observers in registration order.
    pub response_observers: Arc<[Arc<dyn ResponseObserver>]>,
    /// Attempt middleware in registration order.
    pub attempt_middleware: Arc<[Arc<dyn crate::AttemptMiddleware>]>,
}

/// Send-capable image API execution capability.
pub trait ImagesApi: Send + Sync + 'static {
    /// Generates one non-streaming image response. Failures are in-band, as in Pi.
    fn generate(
        &self,
        request: ResolvedImageRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, AssistantImages>;
}

/// Fully resolved single-threaded image request.
pub struct LocalResolvedImageRequest {
    /// Selected image model.
    pub model: ImageModelDescriptor,
    /// Canonical image input.
    pub context: ImageGenerationContext,
    /// Common request controls.
    pub request_options: ApiRequestOptions,
    /// Effective endpoint.
    pub endpoint: Url,
    /// Final logical headers.
    pub headers: HeaderMap,
    /// Credential-derived invariant headers.
    pub auth_headers: HeaderMap,
    /// Provider-resolved environment with request values overlaid per key.
    pub environment: BTreeMap<String, String>,
    /// Provider-specific request metadata preserved from the public options.
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// Resolved API key.
    pub api_key: Option<SecretString>,
    /// Local logical payload transforms in registration order.
    pub payload_transforms: Rc<[Rc<dyn LocalErasedPayloadTransform>]>,
    /// Resolved retry policy.
    pub retry_policy: RetryPolicy,
    /// Per-attempt establishment timeout.
    pub timeout: Option<Duration>,
    /// Local retry classifier.
    pub retry_classifier: Rc<dyn LocalRetryClassifier>,
    /// Local response observers.
    pub response_observers: Rc<[Rc<dyn LocalResponseObserver>]>,
    /// Local attempt middleware.
    pub attempt_middleware: Rc<[Rc<dyn LocalAttemptMiddleware>]>,
}

/// Single-threaded image API execution capability.
pub trait LocalImagesApi: 'static {
    /// Generates one non-streaming image response without requiring `Send`.
    fn generate(
        &self,
        request: LocalResolvedImageRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, AssistantImages>;
}

/// Stable image-catalog error categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageCatalogErrorKind {
    /// Pi's selected-provider `ModelsError("model_source", ...)` category.
    ModelSource,
}

impl ImageCatalogErrorKind {
    /// Returns Pi's stable public code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::ModelSource => "model_source",
        }
    }
}

/// Failure while loading one provider-owned image-model catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageCatalogError {
    /// Stable typed category.
    pub kind: ImageCatalogErrorKind,
    /// Stable Pi-compatible public code.
    pub code: &'static str,
    /// Stable secret-free diagnostic.
    pub message: String,
}

impl ImageCatalogError {
    /// Creates a catalog failure.
    pub fn new(message: impl Into<String>) -> Self {
        let kind = ImageCatalogErrorKind::ModelSource;
        Self {
            kind,
            code: kind.code(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ImageCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ImageCatalogError {}

/// Provider-owned asynchronous image-model source.
pub trait ImageModelCatalogSource: Send + Sync + 'static {
    /// Loads one complete image-model snapshot.
    fn fetch(
        &self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Vec<ImageModelDescriptor>, ImageCatalogError>>;
}

/// Single-threaded provider-owned asynchronous image-model source.
pub trait LocalImageModelCatalogSource: 'static {
    /// Loads one complete local image-model snapshot.
    fn fetch(
        &self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Vec<ImageModelDescriptor>, ImageCatalogError>>;
}

pub(crate) fn image_timestamp_now() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
}

impl fmt::Debug for ResolvedImageRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedImageRequest")
            .field("model", &self.model)
            .field("context", &self.context)
            .field("endpoint", &"<redacted endpoint>")
            .field("headers", &"<redacted headers>")
            .field("api_key", &self.api_key)
            .finish_non_exhaustive()
    }
}
