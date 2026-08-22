//! Executor-neutral HTTP and request-middleware contracts from Architecture
//! v2 part 2 §2.5–§2.6.

use crate::{
    ApiFamily, ApiId, CancellationToken, LocalBoxFuture, LocalBoxStream, ModelDescriptor, ModelRef,
    ProviderId, SendBoxFuture, SendBoxStream, TypedModelDescriptor,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use std::any::Any;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

/// One fully encoded HTTP request passed to an injected transport.
#[derive(Clone, Eq, PartialEq)]
pub struct HttpRequest {
    /// HTTP request method.
    pub method: Method,
    /// Effective endpoint after authentication resolution.
    pub url: Url,
    /// Final attempt-local headers.
    pub headers: HeaderMap,
    /// Frozen logical request body.
    pub body: Vec<u8>,
    /// Per-attempt response-establishment timeout.
    pub timeout: Option<Duration>,
    /// Zero-based transport-attempt number.
    pub attempt: u32,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &"<redacted endpoint>")
            .field("headers", &"<redacted headers>")
            .field("body", &"<redacted body>")
            .field("timeout", &self.timeout)
            .field("attempt", &self.attempt)
            .finish()
    }
}

/// Streaming HTTP response body owned by a provider decoder.
pub type HttpBody = SendBoxStream<'static, Result<Vec<u8>, TransportError>>;

/// Streaming HTTP response body for a single-threaded executor.
pub type LocalHttpBody = LocalBoxStream<'static, Result<Vec<u8>, TransportError>>;

/// An established HTTP response. The body is not polled by the request
/// pipeline before response observers run.
pub struct HttpResponse {
    /// HTTP response status.
    pub status: u16,
    /// Raw response headers.
    pub headers: HeaderMap,
    /// Unconsumed response body.
    pub body: HttpBody,
}

impl HttpResponse {
    /// Creates a response whose body yields one byte chunk.
    pub fn from_bytes(status: u16, headers: HeaderMap, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            body: Box::pin(futures_util::stream::once(async move { Ok(body) })),
        }
    }

    /// Creates a response with no body chunks.
    pub fn empty(status: u16, headers: HeaderMap) -> Self {
        Self {
            status,
            headers,
            body: Box::pin(futures_util::stream::empty()),
        }
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &"<redacted headers>")
            .field("body", &"<stream>")
            .finish()
    }
}

/// An established HTTP response owned by a local provider decoder.
pub struct LocalHttpResponse {
    /// HTTP response status.
    pub status: u16,
    /// Raw response headers.
    pub headers: HeaderMap,
    /// Unconsumed local response body.
    pub body: LocalHttpBody,
}

impl LocalHttpResponse {
    /// Creates a local response whose body yields one byte chunk.
    pub fn from_bytes(status: u16, headers: HeaderMap, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            body: Box::pin(futures_util::stream::once(async move { Ok(body) })),
        }
    }

    /// Creates a local response with no body chunks.
    pub fn empty(status: u16, headers: HeaderMap) -> Self {
        Self {
            status,
            headers,
            body: Box::pin(futures_util::stream::empty()),
        }
    }
}

impl fmt::Debug for LocalHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalHttpResponse")
            .field("status", &self.status)
            .field("headers", &"<redacted headers>")
            .field("body", &"<local stream>")
            .finish()
    }
}

/// Sanitized failure produced by an injected HTTP transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportError {
    /// Stable transport error code.
    pub code: String,
    /// Secret-free diagnostic text.
    pub message: String,
}

impl TransportError {
    /// Creates a transport failure.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TransportError {}

/// Injected HTTP transport, equivalent to Pi's injected `fetch` option.
pub trait HttpTransport: Send + Sync + 'static {
    /// Establishes an HTTP response without consuming its body stream.
    fn execute(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>>;
}

/// Single-threaded injected HTTP transport.
///
/// Implementations and returned futures may retain `Rc`-backed host state.
pub trait LocalHttpTransport: 'static {
    /// Establishes a local HTTP response without consuming its body stream.
    fn execute(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>>;
}

/// Failure from a request middleware callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MiddlewareError {
    /// Stable middleware error code.
    pub code: String,
    /// Secret-free diagnostic text.
    pub message: String,
}

impl MiddlewareError {
    /// Creates a middleware failure.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for MiddlewareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MiddlewareError {}

/// Borrowed identity available to a Models-level header transform.
#[derive(Clone, Copy)]
pub struct HeaderTransformContext<'a> {
    /// Registered provider identity.
    pub provider: &'a ProviderId,
    /// Current catalog model.
    pub model: &'a ModelDescriptor,
    /// API family selected by the model descriptor.
    pub api: &'a ApiId,
    /// Endpoint resolved before compatibility detection.
    pub endpoint: &'a Url,
}

/// Models-level logical-header transformation.
pub trait HeaderTransform: Send + Sync + 'static {
    /// Mutates the case-insensitive logical header map. Calling
    /// [`HeaderMap::remove`] is the explicit deletion operation.
    fn transform<'a>(
        &'a self,
        context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>>;
}

/// Single-threaded Models-level logical-header transformation.
pub trait LocalHeaderTransform: 'static {
    /// Mutates the case-insensitive logical header map.
    fn transform<'a>(
        &'a self,
        context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>>;
}

/// Borrowed inputs available to a typed API-family payload transform.
pub struct PayloadTransformContext<'a, A: ApiFamily + ?Sized> {
    /// Target typed model descriptor.
    pub model: &'a TypedModelDescriptor<A>,
    /// Effective endpoint.
    pub endpoint: &'a Url,
    /// Final logical headers.
    pub headers: &'a HeaderMap,
}

/// Typed API-family payload transformation.
pub trait PayloadTransform<A: ApiFamily>: Send + Sync + 'static {
    /// Mutates or replaces a typed wire request.
    fn transform<'a>(
        &'a self,
        context: PayloadTransformContext<'a, A>,
        payload: &'a mut A::WireRequest,
    ) -> SendBoxFuture<'a, Result<PayloadTransformResult<A::WireRequest>, MiddlewareError>>;
}

/// Single-threaded typed API-family payload transformation.
pub trait LocalPayloadTransform<A: ApiFamily>: 'static {
    /// Mutates or replaces a typed wire request without requiring a `Send`
    /// callback future.
    fn transform<'a>(
        &'a self,
        context: PayloadTransformContext<'a, A>,
        payload: &'a mut A::WireRequest,
    ) -> LocalBoxFuture<'a, Result<PayloadTransformResult<A::WireRequest>, MiddlewareError>>;
}

/// Result of one typed payload transform.
pub enum PayloadTransformResult<T> {
    /// Retain the possibly in-place-mutated payload.
    Continue,
    /// Replace the payload entirely.
    Replace(T),
}

trait ErasedProviderPayloadBody: Send + Sync {
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn encode(&self) -> Result<Vec<u8>, MiddlewareError>;
}

struct RawProviderPayloadBody(Vec<u8>);

impl ErasedProviderPayloadBody for RawProviderPayloadBody {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn encode(&self) -> Result<Vec<u8>, MiddlewareError> {
        Ok(self.0.clone())
    }
}

struct TypedProviderPayloadBody<A: ApiFamily> {
    model: TypedModelDescriptor<A>,
    wire_request: A::WireRequest,
    encoder: Arc<WireEncoder<A>>,
}

type WireEncoder<A> = dyn Fn(&<A as ApiFamily>::WireRequest) -> Result<Vec<u8>, MiddlewareError>
    + Send
    + Sync
    + 'static;

impl<A: ApiFamily> ErasedProviderPayloadBody for TypedProviderPayloadBody<A> {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn encode(&self) -> Result<Vec<u8>, MiddlewareError> {
        (self.encoder)(&self.wire_request)
    }
}

/// Type-erased provider payload at the dynamically dispatched boundary.
///
/// Typed API-family wire requests remain typed until every logical payload
/// transform has run. Only then does the HTTP API freeze exact body bytes for
/// retries. Raw bytes remain available for custom handlers that have no typed
/// middleware.
pub struct ProviderPayload {
    /// HTTP method used for this payload.
    pub method: Method,
    body: Box<dyn ErasedProviderPayloadBody>,
}

impl ProviderPayload {
    /// Creates an already encoded JSON POST payload.
    pub fn json(body_bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            method: Method::POST,
            body: Box::new(RawProviderPayloadBody(body_bytes.into())),
        }
    }

    /// Erases a typed API-family wire request while retaining the type through
    /// logical payload middleware.
    pub fn typed<A, E>(
        method: Method,
        model: TypedModelDescriptor<A>,
        wire_request: A::WireRequest,
        encoder: E,
    ) -> Self
    where
        A: ApiFamily,
        E: Fn(&A::WireRequest) -> Result<Vec<u8>, MiddlewareError> + Send + Sync + 'static,
    {
        Self {
            method,
            body: Box::new(TypedProviderPayloadBody::<A> {
                model,
                wire_request,
                encoder: Arc::new(encoder),
            }),
        }
    }

    /// Erases a JSON-serializable typed wire request.
    pub fn typed_json<A>(model: TypedModelDescriptor<A>, wire_request: A::WireRequest) -> Self
    where
        A: ApiFamily,
        A::WireRequest: serde::Serialize,
    {
        Self::typed(Method::POST, model, wire_request, |wire_request| {
            serde_json::to_vec(wire_request).map_err(|error| {
                MiddlewareError::new(
                    "provider_payload_encode",
                    format!("failed to encode provider payload: {error}"),
                )
            })
        })
    }

    /// Encodes the fully transformed payload into the exact body bytes frozen
    /// across retries.
    pub fn encode_body(&self) -> Result<Vec<u8>, MiddlewareError> {
        self.body.encode()
    }

    fn typed_mut<A: ApiFamily>(&mut self) -> Option<&mut TypedProviderPayloadBody<A>> {
        self.body
            .as_any_mut()
            .downcast_mut::<TypedProviderPayloadBody<A>>()
    }
}

impl fmt::Debug for ProviderPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderPayload")
            .field("method", &self.method)
            .field("body", &"<typed wire request>")
            .finish()
    }
}

/// Borrowed identity available to erased payload middleware.
#[derive(Clone, Copy)]
pub struct ErasedPayloadContext<'a> {
    /// Current model reference.
    pub model: &'a ModelRef,
    /// Selected API family.
    pub api: &'a ApiId,
    /// Effective endpoint.
    pub endpoint: &'a Url,
    /// Final logical headers.
    pub headers: &'a HeaderMap,
}

/// Dynamically dispatched payload transformation.
pub trait ErasedPayloadTransform: Send + Sync + 'static {
    /// Mutates or replaces the encoded provider payload.
    fn transform<'a>(
        &'a self,
        context: ErasedPayloadContext<'a>,
        payload: &'a mut ProviderPayload,
    ) -> SendBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>>;
}

/// Type-erased single-threaded payload transformation.
pub trait LocalErasedPayloadTransform: 'static {
    /// Mutates or replaces the still-typed provider payload.
    fn transform<'a>(
        &'a self,
        context: ErasedPayloadContext<'a>,
        payload: &'a mut ProviderPayload,
    ) -> LocalBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>>;
}

/// Erases a typed Send payload transform for registration in [`crate::Models`].
pub struct PayloadTransformAdapter<A: ApiFamily> {
    inner: Arc<dyn PayloadTransform<A>>,
    marker: PhantomData<fn() -> A>,
}

impl<A: ApiFamily> PayloadTransformAdapter<A> {
    /// Creates an erased adapter around one typed transform.
    pub fn new(inner: Arc<dyn PayloadTransform<A>>) -> Self {
        Self {
            inner,
            marker: PhantomData,
        }
    }
}

impl<A: ApiFamily> ErasedPayloadTransform for PayloadTransformAdapter<A> {
    fn transform<'a>(
        &'a self,
        context: ErasedPayloadContext<'a>,
        payload: &'a mut ProviderPayload,
    ) -> SendBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>> {
        Box::pin(async move {
            if context.api.as_str() != A::API_ID {
                return Ok(PayloadTransformDisposition::Continue);
            }
            let typed = payload.typed_mut::<A>().ok_or_else(|| {
                MiddlewareError::new(
                    "typed_payload_mismatch",
                    format!(
                        "API {} did not retain its typed wire request through payload middleware",
                        A::API_ID
                    ),
                )
            })?;
            let transform_context = PayloadTransformContext {
                model: &typed.model,
                endpoint: context.endpoint,
                headers: context.headers,
            };
            if let PayloadTransformResult::Replace(replacement) = self
                .inner
                .transform(transform_context, &mut typed.wire_request)
                .await?
            {
                typed.wire_request = replacement;
            }
            Ok(PayloadTransformDisposition::Continue)
        })
    }
}

/// Erases a typed local payload transform for registration in
/// [`crate::LocalModels`].
pub struct LocalPayloadTransformAdapter<A: ApiFamily> {
    inner: Rc<dyn LocalPayloadTransform<A>>,
    marker: PhantomData<fn() -> A>,
}

impl<A: ApiFamily> LocalPayloadTransformAdapter<A> {
    /// Creates an erased adapter around one typed local transform.
    pub fn new(inner: Rc<dyn LocalPayloadTransform<A>>) -> Self {
        Self {
            inner,
            marker: PhantomData,
        }
    }
}

impl<A: ApiFamily> LocalErasedPayloadTransform for LocalPayloadTransformAdapter<A> {
    fn transform<'a>(
        &'a self,
        context: ErasedPayloadContext<'a>,
        payload: &'a mut ProviderPayload,
    ) -> LocalBoxFuture<'a, Result<PayloadTransformDisposition, MiddlewareError>> {
        Box::pin(async move {
            if context.api.as_str() != A::API_ID {
                return Ok(PayloadTransformDisposition::Continue);
            }
            let typed = payload.typed_mut::<A>().ok_or_else(|| {
                MiddlewareError::new(
                    "typed_payload_mismatch",
                    format!(
                        "API {} did not retain its typed wire request through local payload middleware",
                        A::API_ID
                    ),
                )
            })?;
            let transform_context = PayloadTransformContext {
                model: &typed.model,
                endpoint: context.endpoint,
                headers: context.headers,
            };
            if let PayloadTransformResult::Replace(replacement) = self
                .inner
                .transform(transform_context, &mut typed.wire_request)
                .await?
            {
                typed.wire_request = replacement;
            }
            Ok(PayloadTransformDisposition::Continue)
        })
    }
}

/// Result of one erased payload transform.
pub enum PayloadTransformDisposition {
    /// Retain the possibly in-place-mutated payload.
    Continue,
    /// Replace the encoded provider payload.
    Replace(ProviderPayload),
}

/// Immutable response metadata observed before body consumption.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderResponseMetadata {
    /// Zero-based transport-attempt number.
    pub attempt: u32,
    /// HTTP status.
    pub status: u16,
    /// Raw response headers.
    pub headers: HeaderMap,
    /// Provider request identifier when exposed in a standard header.
    pub request_id: Option<String>,
}

impl fmt::Debug for ProviderResponseMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderResponseMetadata")
            .field("attempt", &self.attempt)
            .field("status", &self.status)
            .field("headers", &"<redacted headers>")
            .field("request_id", &self.request_id)
            .finish()
    }
}

/// Borrowed request identity supplied to a response observer.
#[derive(Clone, Copy)]
pub struct ResponseObservationContext<'a> {
    /// Current model reference.
    pub model: &'a ModelRef,
    /// Selected API family.
    pub api: &'a ApiId,
    /// Effective endpoint.
    pub endpoint: &'a Url,
}

/// Observer invoked for every established HTTP response.
pub trait ResponseObserver: Send + Sync + 'static {
    /// Observes status and headers before the body stream is consumed.
    fn on_response<'a>(
        &'a self,
        context: ResponseObservationContext<'a>,
        response: &'a ProviderResponseMetadata,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>>;
}

/// Single-threaded response observer.
pub trait LocalResponseObserver: 'static {
    /// Observes status and headers before the local body stream is consumed.
    fn on_response<'a>(
        &'a self,
        context: ResponseObservationContext<'a>,
        response: &'a ProviderResponseMetadata,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>>;
}

/// Attempt-local request middleware. It runs once for every transport attempt,
/// after cloning the frozen logical request.
pub trait AttemptMiddleware: Send + Sync + 'static {
    /// Mutates one attempt-local request.
    fn before_attempt<'a>(
        &'a self,
        attempt: u32,
        request: &'a mut HttpRequest,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>>;
}

/// Single-threaded attempt-local request middleware.
pub trait LocalAttemptMiddleware: 'static {
    /// Mutates one attempt-local request.
    fn before_attempt<'a>(
        &'a self,
        attempt: u32,
        request: &'a mut HttpRequest,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>>;
}

/// Applies one serialized logical-header layer to an HTTP header map.
///
/// Header names are parsed through [`HeaderName`], so replacement and deletion
/// are case-insensitive. A `None` value is an explicit deletion marker.
pub fn apply_header_spec(
    headers: &mut HeaderMap,
    layer: &crate::HeaderMapSpec,
) -> Result<(), MiddlewareError> {
    for (name, value) in layer {
        let parsed_name = HeaderName::try_from(name.as_str()).map_err(|error| {
            MiddlewareError::new(
                "invalid_header_name",
                format!("invalid header name {name:?}: {error}"),
            )
        })?;
        match value {
            Some(value) => {
                let parsed_value = HeaderValue::try_from(value.as_str()).map_err(|error| {
                    MiddlewareError::new(
                        "invalid_header_value",
                        format!("invalid value for header {name:?}: {error}"),
                    )
                })?;
                headers.insert(parsed_name, parsed_value);
            }
            None => {
                headers.remove(parsed_name);
            }
        }
    }
    Ok(())
}

/// Merges an already parsed header layer case-insensitively.
pub fn merge_header_map(target: &mut HeaderMap, layer: &HeaderMap) {
    for (name, value) in layer {
        target.insert(name, value.clone());
    }
}

/// Returns whether a Bedrock logical header is owned by signing/auth.
pub fn is_bedrock_reserved_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    name == "authorization" || name == "host" || name.starts_with("x-amz-")
}

/// Inserts allowed Bedrock logical headers into the serialized Smithy request
/// immediately before signing. Reserved names are silently suppressed to match
/// pinned Pi.
pub fn apply_bedrock_signer_headers(logical: &HeaderMap, serialized: &mut HeaderMap) {
    for (name, value) in logical {
        if !is_bedrock_reserved_header(name) {
            serialized.insert(name, value.clone());
        }
    }
}

/// Extracts the common provider request-id header without interpreting the
/// response body.
pub(crate) fn request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    ["x-request-id", "request-id", "x-amzn-requestid"]
        .into_iter()
        .find_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
}
