use crate::types::{AbortSignal, FetchFunction, ProviderHttpRequest, ProviderHttpResponse};
use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use url::Url;

pub const CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL: &str = "cloudflare-gateway-binding";

#[derive(Debug, Clone, PartialEq)]
pub struct AiGatewayUniversalRequestLike {
    pub provider: String,
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
    pub query: Value,
}

#[derive(Clone, Default)]
pub struct AiGatewayRunOptions {
    pub signal: Option<Arc<dyn AbortSignal>>,
}

pub trait AiGatewayBindingGateway: Send + Sync {
    fn run(
        &self,
        data: AiGatewayUniversalRequestLike,
        options: AiGatewayRunOptions,
    ) -> BoxFuture<'static, Result<ProviderHttpResponse, String>>;
}

pub trait AiGatewayBinding: Send + Sync {
    fn gateway(&self, id: &str) -> Arc<dyn AiGatewayBindingGateway>;
}

#[derive(Clone)]
pub struct GatewayBindingFetchOptions {
    pub binding: Arc<dyn AiGatewayBinding>,
    pub base_url: String,
    pub gateway: String,
}

struct GatewayBindingFetch {
    binding: Arc<dyn AiGatewayBinding>,
    gateway: String,
    base_origin: url::Origin,
    base_origin_text: String,
    base_path: String,
}

impl FetchFunction for GatewayBindingFetch {
    fn fetch(
        &self,
        request: ProviderHttpRequest,
    ) -> BoxFuture<'_, Result<ProviderHttpResponse, String>> {
        let method = request.method.to_uppercase();
        let url = request.url.clone();
        let parsed = Url::parse(&url).ok();
        let in_prefix = parsed.as_ref().is_some_and(|parsed| {
            parsed.origin() == self.base_origin && parsed.path().starts_with(&self.base_path)
        });
        if !in_prefix {
            let message = format!(
                "createGatewayBindingFetch: {method} {url} is outside the configured gateway prefix ({}{}); this fetch only serves its gateway-bound client",
                self.base_origin_text, self.base_path,
            );
            return Box::pin(async move { Err(message) });
        }
        let parsed = parsed.expect("prefix check requires a parsed URL");
        let unexpressible = |reason: &str| {
            format!(
                "createGatewayBindingFetch: cannot express {method} {url} as a universal gateway request ({reason}); route it over HTTPS with gateway auth instead"
            )
        };
        if method != "POST" {
            let message = unexpressible("only POST is supported");
            return Box::pin(async move { Err(message) });
        }
        let rest = &parsed.path()[self.base_path.len()..];
        let Some(slash) = rest.find('/') else {
            let message = unexpressible("missing provider/endpoint path");
            return Box::pin(async move { Err(message) });
        };
        if slash == 0 {
            let message = unexpressible("missing provider/endpoint path");
            return Box::pin(async move { Err(message) });
        }
        let provider = rest[..slash].to_owned();
        let mut endpoint = rest[slash + 1..].to_owned();
        if let Some(query) = parsed.query() {
            endpoint.push('?');
            endpoint.push_str(query);
        }
        let Some(body) = request.body.as_deref() else {
            let message = unexpressible("missing body");
            return Box::pin(async move { Err(message) });
        };
        let body_text = String::from_utf8_lossy(body);
        let query = match serde_json::from_str::<Value>(&body_text) {
            Ok(query) => query,
            Err(_) => {
                let message = unexpressible("non-JSON body");
                return Box::pin(async move { Err(message) });
            }
        };
        let headers = request
            .headers
            .into_iter()
            .filter_map(|(name, value)| {
                let name = name.to_ascii_lowercase();
                (!matches!(
                    name.as_str(),
                    "content-length" | "host" | "cf-aig-authorization"
                ))
                .then_some((name, value))
            })
            .collect();
        let data = AiGatewayUniversalRequestLike {
            provider,
            endpoint,
            headers,
            query,
        };
        let gateway = self.binding.gateway(&self.gateway);
        let options = AiGatewayRunOptions {
            signal: request.signal,
        };
        Box::pin(async move { gateway.run(data, options).await })
    }
}

pub fn create_gateway_binding_fetch(
    options: GatewayBindingFetchOptions,
) -> Result<Arc<dyn FetchFunction>, String> {
    let base = Url::parse(&options.base_url)
        .map_err(|error| format!("Invalid gateway base URL: {error}"))?;
    let base_path = if base.path().ends_with('/') {
        base.path().to_owned()
    } else {
        format!("{}/", base.path())
    };
    Ok(Arc::new(GatewayBindingFetch {
        binding: options.binding,
        gateway: options.gateway,
        base_origin: base.origin(),
        base_origin_text: base.origin().ascii_serialization(),
        base_path,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProviderBodyStream;
    use crate::utils::abort::AbortController;
    use futures::StreamExt;
    use std::sync::{Mutex, PoisonError};

    const BASE_URL: &str = "https://gateway.ai.cloudflare.com/v1/account-id/my-gateway";

    #[derive(Clone)]
    struct CapturedRun {
        gateway_id: String,
        data: AiGatewayUniversalRequestLike,
        has_signal: bool,
    }

    #[derive(Default)]
    struct FakeBinding {
        runs: Arc<Mutex<Vec<CapturedRun>>>,
    }

    struct FakeGateway {
        gateway_id: String,
        runs: Arc<Mutex<Vec<CapturedRun>>>,
    }

    impl AiGatewayBinding for FakeBinding {
        fn gateway(&self, id: &str) -> Arc<dyn AiGatewayBindingGateway> {
            Arc::new(FakeGateway {
                gateway_id: id.to_owned(),
                runs: self.runs.clone(),
            })
        }
    }

    impl AiGatewayBindingGateway for FakeGateway {
        fn run(
            &self,
            data: AiGatewayUniversalRequestLike,
            options: AiGatewayRunOptions,
        ) -> BoxFuture<'static, Result<ProviderHttpResponse, String>> {
            self.runs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(CapturedRun {
                    gateway_id: self.gateway_id.clone(),
                    data,
                    has_signal: options.signal.is_some(),
                });
            let body: ProviderBodyStream =
                futures::stream::once(async { Ok(b"data: {}\n\n".to_vec()) }).boxed();
            Box::pin(async move {
                Ok(ProviderHttpResponse {
                    status: 200,
                    status_text: "OK".to_owned(),
                    headers: BTreeMap::from([("cf-aig-log-id".to_owned(), "log-1".to_owned())]),
                    body: Some(body),
                })
            })
        }
    }

    fn setup() -> (Arc<dyn FetchFunction>, Arc<Mutex<Vec<CapturedRun>>>) {
        let binding = Arc::new(FakeBinding::default());
        let runs = binding.runs.clone();
        let fetch = create_gateway_binding_fetch(GatewayBindingFetchOptions {
            binding,
            base_url: BASE_URL.to_owned(),
            gateway: "my-gateway".to_owned(),
        })
        .expect("binding fetch");
        (fetch, runs)
    }

    fn request(url: impl Into<String>, method: &str, body: &str) -> ProviderHttpRequest {
        ProviderHttpRequest {
            method: method.to_owned(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: Some(body.as_bytes().to_vec()),
            signal: None,
        }
    }

    /// Ports pi `test/cloudflare-gateway-binding.test.ts:32-68,128-200`.
    #[tokio::test]
    async fn derives_universal_request_and_returns_streaming_response_untouched() {
        let (fetch, runs) = setup();
        let mut first = request(
            format!("{BASE_URL}/anthropic/v1/messages"),
            "post",
            r#"{"model":"claude"}"#,
        );
        first
            .headers
            .insert("Anthropic-Version".to_owned(), "2023-06-01".to_owned());
        let response = fetch.fetch(first).await.expect("response");
        let mut second = request(
            format!("{BASE_URL}/openai/responses?beta=true"),
            "POST",
            "{}",
        );
        second.signal = Some(AbortController::new().signal());
        fetch.fetch(second).await.expect("response");
        {
            let captured = runs.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!(captured[0].gateway_id, "my-gateway");
            assert_eq!(captured[0].data.provider, "anthropic");
            assert_eq!(captured[0].data.endpoint, "v1/messages");
            assert_eq!(
                captured[0].data.query,
                serde_json::json!({"model":"claude"})
            );
            assert_eq!(captured[0].data.headers["anthropic-version"], "2023-06-01");
            assert_eq!(captured[1].data.endpoint, "responses?beta=true");
            assert!(captured[1].has_signal);
        }
        assert_eq!(response.headers["cf-aig-log-id"], "log-1");
        let bytes = response
            .body
            .expect("body")
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("chunks")
            .concat();
        assert_eq!(bytes, b"data: {}\n\n");
    }

    /// Ports pi `test/cloudflare-gateway-binding.test.ts:70-126,289-330`.
    #[tokio::test]
    async fn lowercases_collapses_and_strips_only_transport_headers() {
        let (fetch, runs) = setup();
        let mut input = request(format!("{BASE_URL}/anthropic/v1/messages"), "POST", "{}");
        input.headers = BTreeMap::from([
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Content-Length".to_owned(), "17".to_owned()),
            ("Host".to_owned(), "gateway.ai.cloudflare.com".to_owned()),
            (
                "CF-AIG-Authorization".to_owned(),
                format!("Bearer {CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL}"),
            ),
            ("cf-aig-metadata".to_owned(), r#"{"user":"42"}"#.to_owned()),
            ("x-api-key".to_owned(), "provider-key".to_owned()),
        ]);
        fetch.fetch(input).await.expect("response");
        let captured = runs.lock().unwrap_or_else(PoisonError::into_inner);
        let headers = &captured[0].data.headers;
        assert!(!headers.contains_key("cf-aig-authorization"));
        assert!(!headers.contains_key("content-length"));
        assert!(!headers.contains_key("host"));
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["cf-aig-metadata"], r#"{"user":"42"}"#);
        assert_eq!(headers["x-api-key"], "provider-key");
    }

    /// Ports pi `test/cloudflare-gateway-binding.test.ts:203-287`.
    #[tokio::test]
    async fn rejects_unexpressible_and_out_of_prefix_requests_after_url_normalization() {
        let (fetch, runs) = setup();
        for (input, expected) in [
            (
                request(format!("{BASE_URL}/anthropic/v1/messages"), "GET", "{}"),
                "only POST is supported",
            ),
            (
                request(
                    format!("{BASE_URL}/anthropic/v1/messages"),
                    "POST",
                    "not json",
                ),
                "non-JSON body",
            ),
            (
                request(format!("{BASE_URL}/anthropic"), "POST", "{}"),
                "missing provider/endpoint path",
            ),
            (
                request("https://api.openai.com/v1/chat/completions", "POST", "{}"),
                "outside the configured gateway prefix",
            ),
            (
                request(
                    format!("{BASE_URL}/../other-gateway/anthropic/v1/messages"),
                    "POST",
                    "{}",
                ),
                "outside the configured gateway prefix",
            ),
        ] {
            let error = match fetch.fetch(input).await {
                Ok(_) => panic!("request must reject"),
                Err(error) => error,
            };
            assert!(error.contains(expected), "{error}");
        }
        let mut missing_body = request(format!("{BASE_URL}/anthropic/v1/messages"), "POST", "{}");
        missing_body.body = None;
        let error = match fetch.fetch(missing_body).await {
            Ok(_) => panic!("missing body must reject"),
            Err(error) => error,
        };
        assert!(error.contains("missing body"), "{error}");
        let mut lossy_utf8 = request(format!("{BASE_URL}/anthropic/v1/messages"), "POST", "{}");
        lossy_utf8.body = Some(vec![b'"', 0xff, b'"']);
        fetch
            .fetch(lossy_utf8)
            .await
            .expect("lossy UTF-8 JSON text");
        fetch
            .fetch(request(
                format!("{BASE_URL}/anthropic/../anthropic/v1/./messages"),
                "POST",
                "{}",
            ))
            .await
            .expect("normalized path");
        let captured = runs.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].data.query, Value::String("�".to_owned()));
        assert_eq!(captured[1].data.provider, "anthropic");
        assert_eq!(captured[1].data.endpoint, "v1/messages");
        assert!(!captured[1].has_signal);
    }
}
