//! Authenticated HTTP/SSE execution for the proxy provider.
//!
//! A request is converted to the stable version-one DTO, posted with bearer authentication, and
//! folded from compact SSE events into partial-preserving assistant events. Whitespace-only SSE
//! `data` fields are ignored. HTTP, transport, decode, protocol, resource-limit, server-reported,
//! and cancellation failures resolve in-band; the returned assistant stream fuses after its first
//! terminal event.
//!
//! HTTP error diagnostics are bounded, and progressive tool arguments have the limits documented
//! by [`super::ProxyAssistantMessageEvent`]. SSE event/text framing and accumulated assistant text
//! are still unbounded, so callers must use HTTPS in production and treat the endpoint as trusted.

use super::accumulator::ProxyAccumulator;
use super::{ProxyConfigError, ProxyRequestV1, ProxyStreamOptions};
use crate::{
    AssistantMessageEventStream, StreamFn, StreamRequest, StreamResponseInfo, header_pairs,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::{ModelIden, ModelSpec};
use std::collections::VecDeque;
use tokio_util::sync::CancellationToken;

/// Maximum number of bytes inspected for an HTTP error-message override.
///
/// Non-JSON bodies are never echoed. Capping this diagnostic read also prevents a failed proxy
/// response from becoming an unbounded allocation before the in-band terminal can resolve.
const MAX_HTTP_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Drop-in proxy implementation of [`StreamFn`].
///
/// Clones retain the validated endpoint, bearer token, and cloneable HTTP client. The token remains
/// redacted by `Debug`. Options created by [`Self::new`] disable redirects; [`Self::with_client`]
/// installs the caller's client and therefore its redirect policy.
///
/// Unlike [`crate::GenaiStreamFn`], this stream function honors the per-request
/// [`StreamRequest::on_payload`]/[`StreamRequest::on_response`] exec hooks directly; see
/// [`stream_proxy`] for their exact semantics.
#[derive(Clone)]
pub struct ProxyStreamFn {
    options: ProxyStreamOptions,
}

impl ProxyStreamFn {
    /// Validate a proxy base URL and bearer token and build a stream provider.
    ///
    /// This uses [`ProxyStreamOptions::new`], including URL-userinfo rejection, `/api/stream`
    /// normalization, token validation, and a default HTTP client with redirects disabled. Use an
    /// HTTPS base URL in production.
    ///
    /// # Errors
    ///
    /// Returns [`ProxyConfigError`] when the URL, token, or built-in client is invalid.
    pub fn new(
        base_url: impl AsRef<str>,
        auth_token: impl Into<String>,
    ) -> Result<Self, ProxyConfigError> {
        ProxyStreamOptions::new(base_url, auth_token).map(Self::from_options)
    }

    /// Build a stream provider from already validated connection options.
    pub fn from_options(options: ProxyStreamOptions) -> Self {
        Self { options }
    }

    /// Replace the HTTP client used for subsequent proxy invocations.
    ///
    /// This also replaces the built-in no-redirect policy. The supplied client retains its
    /// caller-selected redirect and TLS policies; the caller is responsible for keeping bearer
    /// authentication confined to a trusted HTTPS endpoint.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.options = self.options.with_client(client);
        self
    }

    /// Borrow the validated endpoint, secret-bearing token state, and HTTP-client configuration.
    ///
    /// The bearer token has no public accessor and remains redacted from `Debug` output.
    pub fn options(&self) -> &ProxyStreamOptions {
        &self.options
    }
}

impl std::fmt::Debug for ProxyStreamFn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxyStreamFn")
            .field("options", &self.options)
            .finish()
    }
}

#[async_trait]
impl StreamFn for ProxyStreamFn {
    async fn stream(&self, request: StreamRequest) -> AssistantMessageEventStream {
        stream_proxy(request, self.options.clone()).await
    }
}

/// Proxy one provider invocation over an authenticated JSON POST and compact-event SSE response.
///
/// `request` is first converted to [`ProxyRequestV1`]; an unrepresentable `ModelSpec::Target`
/// becomes an in-band error without sending a request. The configured bearer token is attached only
/// as transport authentication and is absent from the JSON DTO.
///
/// The per-request exec hooks on [`StreamRequest`] are honored directly: `on_payload` receives the
/// serialized wire body before send and a `Some` return replaces what is posted (the hook never
/// sees the bearer token, which is transport-level only), and `on_response` observes the HTTP
/// response status and headers before the SSE body — or a non-success error body — is consumed.
///
/// Whitespace-only SSE `data` fields are ignored. HTTP, transport, event decoding, protocol,
/// resource-limit, server-reported, and cancellation failures all become terminal assistant events;
/// cancellation uses `StopReason::Aborted`, accumulated content is preserved, and the returned
/// [`AssistantMessageEventStream`] permanently fuses after its first terminal event.
///
/// Progressive tool arguments are bounded to 128 JSON nesting levels, 1 MiB of raw JSON and 4,096
/// deltas per tool call, and 16 MiB of cumulative reparse work per invocation. SSE framing and
/// assistant text accumulation remain unbounded, so this is not a sandbox for an untrusted server.
/// Use HTTPS in production. This provider boundary does not return runtime errors or panic on server
/// input.
pub async fn stream_proxy(
    request: StreamRequest,
    options: ProxyStreamOptions,
) -> AssistantMessageEventStream {
    let error_model = model_iden_for_error(&request.model);
    let wire_request = match ProxyRequestV1::try_from(&request) {
        Ok(wire_request) => wire_request,
        Err(error) => return ProxyAccumulator::request_error(error_model, error.to_string()),
    };
    let cancel = request.cancel.clone();

    let http_request = options
        .client()
        .post(options.endpoint().clone())
        .bearer_auth(options.auth_token())
        .header(reqwest::header::ACCEPT, "text/event-stream");
    // Per-request `on_payload` exec hook: the serialized wire body (which already excludes the
    // bearer token and any resolved-target credentials) is handed to the hook by value, and a
    // `Some` return replaces what is sent. Without a hook the DTO is serialized directly,
    // keeping the pre-hook wire bytes unchanged.
    let http_request = match &request.on_payload {
        Some(on_payload) => {
            let payload = match serde_json::to_value(&wire_request) {
                Ok(payload) => payload,
                Err(error) => {
                    return ProxyAccumulator::request_error(
                        error_model,
                        format!("Failed to serialize proxy request: {error}"),
                    );
                }
            };
            let payload = on_payload(payload.clone(), error_model.clone())
                .await
                .unwrap_or(payload);
            http_request.json(&payload)
        }
        None => http_request.json(&wire_request),
    };

    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => return aborted_stream(error_model),
        response = http_request.send() => response,
    };
    let response = match response {
        Ok(_response) if cancel.is_cancelled() => return aborted_stream(error_model),
        Ok(response) => response,
        Err(_) if cancel.is_cancelled() => return aborted_stream(error_model),
        Err(error) => {
            return ProxyAccumulator::request_error(
                error_model,
                format!("Proxy connection failed: {error}"),
            );
        }
    };

    // Per-request `on_response` exec hook: observe the response head (status + headers, never
    // the body) before any body/stream consumption, including on non-success statuses.
    if let Some(on_response) = &request.on_response {
        let info =
            StreamResponseInfo::new(response.status().as_u16(), header_pairs(response.headers()));
        on_response(info, error_model.clone()).await;
    }

    if !response.status().is_success() {
        let status = response.status();
        let body = read_bounded_error_body(response, &cancel).await;
        if matches!(body, ErrorBodyRead::Aborted) || cancel.is_cancelled() {
            return aborted_stream(error_model);
        }
        let override_message = match body {
            ErrorBodyRead::Complete(body) => proxy_error_override(&body),
            ErrorBodyRead::Unavailable | ErrorBodyRead::Aborted => None,
        };
        let message = override_message
            .map(|message| format!("Proxy error: {message}"))
            .unwrap_or_else(|| format!("Proxy error: {status}"));
        return ProxyAccumulator::request_error(error_model, message);
    }

    if cancel.is_cancelled() {
        return aborted_stream(error_model);
    }

    let upstream = Box::pin(response.bytes_stream().eventsource());
    let accumulator = ProxyAccumulator::new(error_model);
    let state = (upstream, accumulator, cancel, VecDeque::new(), false);
    let stream = futures::stream::unfold(
        state,
        |(mut upstream, mut accumulator, cancel, mut pending, mut finished)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    return Some((event, (upstream, accumulator, cancel, pending, finished)));
                }
                if finished {
                    return None;
                }

                let events = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => accumulator.abort(),
                    item = upstream.next() => match item {
                        Some(Ok(event)) if event.data.trim().is_empty() => Vec::new(),
                        Some(Ok(event)) => {
                            match serde_json::from_str(&event.data) {
                                Ok(event) => accumulator.fold(event),
                                Err(error) => accumulator.fail(format!(
                                    "Failed to decode proxy event JSON: {error}"
                                )),
                            }
                        }
                        Some(Err(error)) => accumulator.fail(format!(
                            "Proxy SSE stream error: {error}"
                        )),
                        None => accumulator.finish_without_terminal(),
                    },
                };
                finished = accumulator.is_terminal();
                pending.extend(events);
            }
        },
    );

    AssistantMessageEventStream::from_stream(stream)
}

fn aborted_stream(model: ModelIden) -> AssistantMessageEventStream {
    let mut accumulator = ProxyAccumulator::new(model);
    AssistantMessageEventStream::from_events(accumulator.abort())
}

enum ErrorBodyRead {
    Complete(Vec<u8>),
    Unavailable,
    Aborted,
}

async fn read_bounded_error_body(
    response: reqwest::Response,
    cancel: &CancellationToken,
) -> ErrorBodyRead {
    let mut chunks = response.bytes_stream();
    let mut body = Vec::new();

    while body.len() < MAX_HTTP_ERROR_BODY_BYTES {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return ErrorBodyRead::Aborted,
            chunk = chunks.next() => chunk,
        };
        match chunk {
            Some(Ok(chunk)) => {
                let remaining = MAX_HTTP_ERROR_BODY_BYTES - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if chunk.len() > remaining {
                    break;
                }
            }
            Some(Err(_)) => return ErrorBodyRead::Unavailable,
            None => break,
        }
    }

    ErrorBodyRead::Complete(body)
}

fn proxy_error_override(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .filter(|message| !message.is_empty())
        .map(ToOwned::to_owned)
}

fn model_iden_for_error(model: &ModelSpec) -> ModelIden {
    match model {
        ModelSpec::Iden(model) => model.clone(),
        ModelSpec::Target(target) => target.model.clone(),
        ModelSpec::Name(name) => {
            let name = name.to_string();
            ModelIden::new(
                AdapterKind::from_model(&name).unwrap_or(AdapterKind::Ollama),
                name,
            )
        }
    }
}
