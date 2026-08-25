//! Cloudflare Workers AI binding transport for universal gateway requests.

use http::{HeaderMap, Method};
use pi_ai::{
    CancellationToken, HttpRequest, HttpResponse, HttpTransport, LocalBoxFuture, LocalHttpResponse,
    LocalHttpTransport, SendBoxFuture, TransportError,
};
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

/// Placeholder satisfying API-family auth checks for pre-authenticated binding calls.
pub const CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL: &str = "cloudflare-gateway-binding";

/// One request accepted by the Workers AI binding universal endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct GatewayBindingRequest {
    /// Universal-endpoint provider segment.
    pub provider: String,
    /// Provider endpoint, including its query string.
    pub endpoint: String,
    /// Lowercase request headers after derived/gateway auth removal.
    pub headers: HeaderMap,
    /// Parsed JSON request body.
    pub query: serde_json::Value,
}

/// Send-capable structural binding surface.
pub trait GatewayBinding: Send + Sync + 'static {
    /// Runs one universal gateway request and returns its native streaming response.
    fn run(
        &self,
        gateway: String,
        request: GatewayBindingRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>>;
}

/// Local-executor structural binding surface.
pub trait LocalGatewayBinding: 'static {
    /// Runs one local universal gateway request.
    fn run(
        &self,
        gateway: String,
        request: GatewayBindingRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>>;
}

/// Strict transport translating a gateway HTTPS prefix to a Workers AI binding.
pub struct GatewayBindingTransport {
    binding: Arc<dyn GatewayBinding>,
    base_url: Url,
    gateway: String,
}

impl GatewayBindingTransport {
    /// Creates a transport for exactly one normalized gateway prefix.
    pub fn new(
        binding: Arc<dyn GatewayBinding>,
        base_url: Url,
        gateway: impl Into<String>,
    ) -> Self {
        Self {
            binding,
            base_url,
            gateway: gateway.into(),
        }
    }
}

impl std::fmt::Debug for GatewayBindingTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayBindingTransport")
            .field("base_url", &"<gateway prefix>")
            .field("gateway", &self.gateway)
            .finish_non_exhaustive()
    }
}

impl HttpTransport for GatewayBindingTransport {
    fn execute(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let request = translate(&self.base_url, request)?;
            self.binding
                .run(self.gateway.clone(), request, cancellation)
                .await
        })
    }
}

/// Local counterpart to [`GatewayBindingTransport`].
pub struct LocalGatewayBindingTransport {
    binding: Rc<dyn LocalGatewayBinding>,
    base_url: Url,
    gateway: String,
}

impl LocalGatewayBindingTransport {
    /// Creates a local transport for exactly one gateway prefix.
    pub fn new(
        binding: Rc<dyn LocalGatewayBinding>,
        base_url: Url,
        gateway: impl Into<String>,
    ) -> Self {
        Self {
            binding,
            base_url,
            gateway: gateway.into(),
        }
    }
}

impl std::fmt::Debug for LocalGatewayBindingTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalGatewayBindingTransport")
            .field("base_url", &"<gateway prefix>")
            .field("gateway", &self.gateway)
            .finish_non_exhaustive()
    }
}

impl LocalHttpTransport for LocalGatewayBindingTransport {
    fn execute(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async move {
            let request = translate(&self.base_url, request)?;
            self.binding
                .run(self.gateway.clone(), request, cancellation)
                .await
        })
    }
}

#[allow(
    clippy::result_large_err,
    reason = "the transport boundary must preserve pi-ai's structured TransportError"
)]
fn translate(base: &Url, request: HttpRequest) -> Result<GatewayBindingRequest, TransportError> {
    let base_path = format!("{}/", base.path().trim_end_matches('/'));
    if request.url.origin() != base.origin() || !request.url.path().starts_with(&base_path) {
        return Err(binding_error(format!(
            "{} {} is outside the configured gateway prefix",
            request.method, request.url
        )));
    }
    if request.method != Method::POST {
        return Err(binding_error(format!(
            "cannot express {} {}: only POST is supported",
            request.method, request.url
        )));
    }
    let rest = &request.url.path()[base_path.len()..];
    let Some((provider, endpoint_path)) = rest.split_once('/') else {
        return Err(binding_error(format!(
            "cannot express POST {}: missing provider/endpoint path",
            request.url
        )));
    };
    if provider.is_empty() || endpoint_path.is_empty() {
        return Err(binding_error(format!(
            "cannot express POST {}: missing provider/endpoint path",
            request.url
        )));
    }
    let query = serde_json::from_slice(&request.body).map_err(|_| {
        binding_error(format!(
            "cannot express POST {}: non-JSON body",
            request.url
        ))
    })?;
    let endpoint = request.url.query().map_or_else(
        || endpoint_path.to_owned(),
        |query| format!("{endpoint_path}?{query}"),
    );
    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        if !matches!(
            name.as_str(),
            "content-length" | "host" | "cf-aig-authorization"
        ) {
            headers.insert(name.clone(), value.clone());
        }
    }
    Ok(GatewayBindingRequest {
        provider: provider.to_owned(),
        endpoint,
        headers,
        query,
    })
}

fn binding_error(message: impl Into<String>) -> TransportError {
    TransportError::new("cloudflare_gateway_binding", message)
}
