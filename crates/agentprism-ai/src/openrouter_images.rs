//! OpenRouter image request lowering and non-streaming response decoding.

use crate::{
    AssistantImages, AttemptFailure, CacheWriteRetention, CancellationToken, Currency, HttpRequest,
    HttpResponse, HttpTransport, ImageGenerationContent, ImageGenerationContext,
    ImageGenerationStopReason, ImageModality, ImageModelDescriptor, ImagesApi, LocalHttpResponse,
    LocalHttpTransport, LocalImagesApi, LocalResolvedImageRequest, LocalRetryClassifier,
    MiddlewareError, OrderedJsonArray, OrderedJsonObject, OrderedJsonValue, OrderedJsonWriter,
    PayloadTransformDisposition, ProviderPayload, ProviderResponseMetadata, ResolvedImageRequest,
    ResponseObservationContext, RetryClassifier, RetryDecision, RetryPolicy, SendBoxFuture, Usage,
    UsageSource, request_id_from_headers,
};
use futures_util::{StreamExt, future::Either};
use http::header;
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

/// Lowers one OpenRouter image request to Pi/`JSON.stringify`-exact bytes.
pub fn encode_openrouter_images_request(
    model: &ImageModelDescriptor,
    context: &ImageGenerationContext,
) -> Result<Vec<u8>, MiddlewareError> {
    let content = context
        .input
        .iter()
        .map(|item| match item {
            ImageGenerationContent::Text { text } => {
                OrderedJsonValue::from(OrderedJsonObject::from_iter([
                    ("type", OrderedJsonValue::from("text")),
                    ("text", OrderedJsonValue::from(text.as_str())),
                ]))
            }
            ImageGenerationContent::Image { data, mime_type } => {
                OrderedJsonValue::from(OrderedJsonObject::from_iter([
                    ("type", OrderedJsonValue::from("image_url")),
                    (
                        "image_url",
                        OrderedJsonValue::from(OrderedJsonObject::from_iter([(
                            "url",
                            OrderedJsonValue::from(format!("data:{mime_type};base64,{data}")),
                        )])),
                    ),
                ]))
            }
        })
        .collect::<OrderedJsonArray>();
    let messages =
        OrderedJsonArray::from_iter([OrderedJsonValue::from(OrderedJsonObject::from_iter([
            ("role", OrderedJsonValue::from("user")),
            ("content", OrderedJsonValue::from(content)),
        ]))]);
    let modalities = if model.output.contains(&ImageModality::Text) {
        ["image", "text"].as_slice()
    } else {
        ["image"].as_slice()
    };
    let modalities = modalities
        .iter()
        .map(|value| OrderedJsonValue::from(*value))
        .collect::<OrderedJsonArray>();
    let body = OrderedJsonValue::from(OrderedJsonObject::from_iter([
        (
            "model",
            OrderedJsonValue::from(model.model_ref.model.as_str()),
        ),
        ("messages", OrderedJsonValue::from(messages)),
        ("stream", OrderedJsonValue::from(false)),
        ("modalities", OrderedJsonValue::from(modalities)),
    ]));
    OrderedJsonWriter::to_vec(&body).map_err(|error| {
        MiddlewareError::new(
            "openrouter_images_encode",
            format!("failed to encode OpenRouter image request: {error}"),
        )
    })
}

/// Decodes Pi's first-choice OpenRouter image response projection.
pub fn decode_openrouter_images_response(
    model: &ImageModelDescriptor,
    body: &[u8],
) -> Result<AssistantImages, String> {
    let response = parse_openrouter_images_json_response(body)?;
    project_openrouter_images_response_at(
        model,
        &OpenRouterImagesSdkResponse::Json(response),
        crate::images::image_timestamp_now(),
    )
    .map_err(|failure| failure.message)
}

fn parse_openrouter_images_json_response(body: &[u8]) -> Result<serde_json::Value, String> {
    // Fetch's `Response.json()` decodes the response body as UTF-8 with
    // replacement before JSON parsing. OpenAI SDK 6.40.0 delegates to that
    // operation, so malformed UTF-8 inside a JSON string is not a parse error.
    let text = String::from_utf8_lossy(body);
    serde_json::from_str(&text)
        .map_err(|error| format!("failed to decode OpenRouter image response: {error}"))
}

enum OpenRouterImagesSdkResponse {
    Json(serde_json::Value),
    Text(String),
    Null,
    Undefined,
}

fn parse_openrouter_images_sdk_response(
    headers: &http::HeaderMap,
    body: Vec<u8>,
) -> Result<OpenRouterImagesSdkResponse, String> {
    if !openrouter_images_sdk_media_is_json(headers) {
        return Ok(OpenRouterImagesSdkResponse::Text(
            String::from_utf8_lossy(&body).into_owned(),
        ));
    }
    parse_openrouter_images_json_response(&body).map(OpenRouterImagesSdkResponse::Json)
}

fn openrouter_images_sdk_media_is_json(headers: &http::HeaderMap) -> bool {
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    media_type.is_some_and(|media_type| {
        media_type.contains("application/json") || media_type.ends_with("+json")
    })
}

fn openrouter_images_sdk_bodyless_response(
    status: u16,
    headers: &http::HeaderMap,
) -> Option<OpenRouterImagesSdkResponse> {
    // OpenAI SDK 6.40.0's defaultParseResponse maps 204 directly to null and
    // JSON responses with an exact Content-Length of zero to undefined. Both
    // checks happen before the response body is polled.
    if status == 204 {
        return Some(OpenRouterImagesSdkResponse::Null);
    }
    (openrouter_images_sdk_media_is_json(headers)
        && headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            == Some("0"))
    .then_some(OpenRouterImagesSdkResponse::Undefined)
}

fn project_openrouter_images_response_at(
    model: &ImageModelDescriptor,
    response: &OpenRouterImagesSdkResponse,
    timestamp: crate::Timestamp,
) -> Result<AssistantImages, OpenRouterImagesProjectionFailure> {
    // Pi allocates this value outside the try/catch and mutates it as response
    // projection advances. A later malformed image therefore retains response
    // metadata, usage, cost, and any text already appended.
    let mut result = AssistantImages::empty(model);
    result.timestamp = timestamp;
    let response = match response {
        OpenRouterImagesSdkResponse::Json(response) => response,
        OpenRouterImagesSdkResponse::Text(text) => {
            return Err(OpenRouterImagesProjectionFailure::new(
                result,
                format!("OpenRouter image SDK response is text, not an object: {text}"),
            ));
        }
        OpenRouterImagesSdkResponse::Null => {
            return Err(OpenRouterImagesProjectionFailure::new(
                result,
                "OpenRouter image SDK response is null",
            ));
        }
        OpenRouterImagesSdkResponse::Undefined => {
            return Err(OpenRouterImagesProjectionFailure::new(
                result,
                "OpenRouter image SDK response is undefined",
            ));
        }
    };
    let Some(response) = response.as_object() else {
        return Err(OpenRouterImagesProjectionFailure::new(
            result,
            "OpenRouter image response is not an object",
        ));
    };
    result.response_id = response
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    if let Some(raw_usage) = response
        .get("usage")
        .filter(|usage| javascript_truthy(usage))
    {
        let usage = parse_usage(raw_usage);
        result.cost = model
            .pricing
            .calculate_cost(&usage, Currency::usd(), CacheWriteRetention::Default)
            .ok();
        result.usage = Some(usage);
    }

    let Some(choices) = response
        .get("choices")
        .and_then(serde_json::Value::as_array)
    else {
        return Err(OpenRouterImagesProjectionFailure::new(
            result,
            "OpenRouter image response omits choices",
        ));
    };
    let Some(choice) = choices.first().filter(|choice| javascript_truthy(choice)) else {
        return Ok(result);
    };
    let Some(message) = choice
        .as_object()
        .and_then(|choice| choice.get("message"))
        .and_then(serde_json::Value::as_object)
    else {
        return Err(OpenRouterImagesProjectionFailure::new(
            result,
            "OpenRouter image response choice omits message",
        ));
    };
    if let Some(text) = message
        .get("content")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
    {
        result.output.push(ImageGenerationContent::text(text));
    }
    let images = match message.get("images") {
        None | Some(serde_json::Value::Null) => None,
        Some(images) => {
            let Some(images) = images.as_array() else {
                return Err(OpenRouterImagesProjectionFailure::new(
                    result,
                    "OpenRouter image response images is not an array",
                ));
            };
            Some(images)
        }
    };
    if let Some(images) = images {
        for image in images {
            // JavaScript property access on every primitive is permitted except
            // null/undefined. JSON has no undefined array entry, so null is the
            // one malformed entry that must take Pi's catch path.
            if image.is_null() {
                return Err(OpenRouterImagesProjectionFailure::new(
                    result,
                    "Cannot read properties of null (reading 'image_url')",
                ));
            }
            let url = image.get("image_url").and_then(|value| {
                value.as_str().or_else(|| {
                    value
                        .as_object()
                        .and_then(|object| object.get("url"))
                        .and_then(serde_json::Value::as_str)
                })
            });
            let Some((mime_type, data)) = url.and_then(parse_base64_data_url) else {
                continue;
            };
            result
                .output
                .push(ImageGenerationContent::image(data, mime_type));
        }
    }
    Ok(result)
}

struct OpenRouterImagesProjectionFailure {
    partial: Box<AssistantImages>,
    message: String,
}

impl OpenRouterImagesProjectionFailure {
    fn new(partial: AssistantImages, message: impl Into<String>) -> Self {
        Self {
            partial: Box::new(partial),
            message: message.into(),
        }
    }
}

fn javascript_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Number(value) => value
            .as_f64()
            .is_some_and(|value| value != 0.0 && !value.is_nan()),
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => true,
    }
}

fn parse_base64_data_url(value: &str) -> Option<(&str, &str)> {
    let value = value.strip_prefix("data:")?;
    let (mime_type, data) = value.split_once(";base64,")?;
    if mime_type.is_empty() || mime_type.contains(';') {
        return None;
    }

    // Pi uses /^data:([^;]+);base64,(.+)$/ without dotAll. Preserve its
    // rejection of any line terminator in the captured payload rather than
    // treating arbitrary multiline data as an embedded image.
    (!data.is_empty() && !data.chars().any(is_javascript_line_terminator))
        .then_some((mime_type, data))
}

fn is_javascript_line_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn number(value: Option<&serde_json::Value>) -> i64 {
    value
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value != 0)
        .unwrap_or(0)
}

fn nonnegative(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(u64::MAX)
}

fn parse_usage(value: &serde_json::Value) -> Usage {
    let prompt = number(value.get("prompt_tokens"));
    let cached = number(
        value
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens")),
    );
    let cache_write = number(
        value
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cache_write_tokens")),
    );
    let cache_read = if cache_write > 0 {
        (cached - cache_write).max(0)
    } else {
        cached
    };
    let input = (prompt - cache_read - cache_write).max(0);
    let output = number(value.get("completion_tokens"));
    Usage {
        input_tokens: nonnegative(input),
        output_tokens: nonnegative(output),
        reasoning_tokens: None,
        cache_read_tokens: Some(nonnegative(cache_read)),
        cache_write_tokens: Some(nonnegative(cache_write)),
        cache_write_one_hour_tokens: None,
        total_tokens: Some(nonnegative(input + output + cache_read + cache_write)),
        source: UsageSource::ProviderReported,
    }
}

/// Send-capable OpenRouter Images API adapter over an injected transport.
pub struct OpenRouterImagesApi {
    transport: Arc<dyn HttpTransport>,
}

impl OpenRouterImagesApi {
    /// Creates an adapter whose transport is the equivalent of Pi's `fetch` option.
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }
}

impl ImagesApi for OpenRouterImagesApi {
    fn generate(
        &self,
        request: ResolvedImageRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, AssistantImages> {
        let transport = Arc::clone(&self.transport);
        Box::pin(async move { execute_openrouter_images(transport, request, cancellation).await })
    }
}

/// Local-executor OpenRouter Images API adapter.
pub struct LocalOpenRouterImagesApi {
    transport: Rc<dyn LocalHttpTransport>,
}

impl LocalOpenRouterImagesApi {
    /// Creates a local adapter over an injected transport.
    pub fn new(transport: Rc<dyn LocalHttpTransport>) -> Self {
        Self { transport }
    }
}

impl LocalImagesApi for LocalOpenRouterImagesApi {
    fn generate(
        &self,
        request: LocalResolvedImageRequest,
        cancellation: CancellationToken,
    ) -> crate::LocalBoxFuture<'_, AssistantImages> {
        let transport = Rc::clone(&self.transport);
        Box::pin(
            async move { execute_local_openrouter_images(transport, request, cancellation).await },
        )
    }
}

struct OpenRouterImagesRetryClassifier(Arc<dyn RetryClassifier>);

impl RetryClassifier for OpenRouterImagesRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision {
        if is_openrouter_sdk_parse_failure(failure) {
            RetryDecision::DoNotRetry
        } else {
            self.0.classify(failure, policy)
        }
    }

    fn normalize_terminal(&self, failure: AttemptFailure) -> AttemptFailure {
        failure
    }
}

struct LocalOpenRouterImagesRetryClassifier(Rc<dyn LocalRetryClassifier>);

impl LocalRetryClassifier for LocalOpenRouterImagesRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision {
        if is_openrouter_sdk_parse_failure(failure) {
            RetryDecision::DoNotRetry
        } else {
            self.0.classify(failure, policy)
        }
    }

    fn normalize_terminal(&self, failure: AttemptFailure) -> AttemptFailure {
        failure
    }
}

fn is_openrouter_sdk_parse_failure(failure: &AttemptFailure) -> bool {
    // OpenAI SDK 6.40.0 exposes these as ordinary errors without the
    // `status`/`headers` pair required by Pi's `isProviderError` guard.
    matches!(
        failure,
        AttemptFailure::Transport { source, .. }
            if matches!(source.code.as_str(), "response_body" | "response_decode")
    )
}

async fn execute_openrouter_images(
    transport: Arc<dyn HttpTransport>,
    request: ResolvedImageRequest,
    cancellation: CancellationToken,
) -> AssistantImages {
    let timestamp = crate::images::image_timestamp_now();
    if !has_openrouter_api_key(request.api_key.as_ref()) {
        return terminal_image_failure_at(
            &request.model,
            &cancellation,
            false,
            format!(
                "No API key for provider: {}",
                request.model.model_ref.provider
            ),
            timestamp,
        );
    }
    let body = match encode_openrouter_images_request(&request.model, &request.context) {
        Ok(body) => body,
        Err(error) => {
            return terminal_image_failure_at(
                &request.model,
                &cancellation,
                false,
                error.to_string(),
                timestamp,
            );
        }
    };
    let url = openrouter_chat_completions_url(&request.endpoint);
    let mut headers = request.headers.clone();
    if let Err(error) = seed_openrouter_sdk_authorization(
        &mut headers,
        request.api_key.as_ref().expect("API key checked above"),
    ) {
        return terminal_image_failure_at(&request.model, &cancellation, false, error, timestamp);
    }
    headers
        .entry(header::ACCEPT)
        .or_insert(http::HeaderValue::from_static("application/json"));
    headers
        .entry(header::CONTENT_TYPE)
        .or_insert(http::HeaderValue::from_static("application/json"));
    let payload_context = crate::ErasedPayloadContext {
        model: &request.model.model_ref,
        api: &request.model.api,
        endpoint: &url,
        headers: &headers,
    };
    let mut payload = ProviderPayload::json(body);
    for transform in request.payload_transforms.iter() {
        match transform.transform(payload_context, &mut payload).await {
            Ok(PayloadTransformDisposition::Continue) => {}
            Ok(PayloadTransformDisposition::Replace(replacement)) => payload = replacement,
            Err(error) => {
                return terminal_image_failure_at(
                    &request.model,
                    &cancellation,
                    false,
                    redact_image_error(&error.to_string(), &request),
                    timestamp,
                );
            }
        }
    }
    let method = payload.method.clone();
    let body = match payload.encode_body() {
        Ok(body) => body,
        Err(error) => {
            return terminal_image_failure_at(
                &request.model,
                &cancellation,
                false,
                redact_image_error(&error.to_string(), &request),
                timestamp,
            );
        }
    };
    let frozen = HttpRequest {
        method,
        url: url.clone(),
        headers,
        auth_headers: request.auth_headers.clone(),
        session_id: None,
        body,
        timeout: request.timeout,
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    };
    let classifier = OpenRouterImagesRetryClassifier(Arc::clone(&request.retry_classifier));
    let response = establish_openrouter_images_with_retry(
        &request.retry_policy,
        &classifier,
        &cancellation,
        |attempt| {
            let mut attempt_request = frozen.clone();
            let invariant_headers = frozen.auth_headers.clone();
            let transport = Arc::clone(&transport);
            let middleware = Arc::clone(&request.attempt_middleware);
            let cancellation = cancellation.clone();
            async move {
                attempt_request.attempt = attempt;
                for item in middleware.iter() {
                    item.before_attempt(attempt, &mut attempt_request)
                        .await
                        .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                }
                attempt_request.auth_headers = invariant_headers;
                let mut response =
                    execute_http_attempt(transport.as_ref(), attempt_request, cancellation.clone())
                        .await?;
                let metadata = ProviderResponseMetadata {
                    attempt,
                    status: response.status,
                    headers: response.headers.clone(),
                    request_id: request_id_from_headers(&response.headers),
                };
                let notify_observers = response.notify_observers;
                if !(200..300).contains(&response.status) {
                    return Err(send_image_http_failure(attempt, response, &cancellation).await);
                }
                let response = if let Some(response) =
                    openrouter_images_sdk_bodyless_response(response.status, &response.headers)
                {
                    response
                } else {
                    let bytes = collect_send_body(&mut response, &cancellation)
                        .await
                        .map_err(|message| {
                            if cancellation.is_cancelled() {
                                AttemptFailure::Cancelled
                            } else {
                                AttemptFailure::transport(
                                    attempt,
                                    crate::TransportError::new("response_body", message),
                                )
                            }
                        })?;
                    parse_openrouter_images_sdk_response(&response.headers, bytes).map_err(
                        |message| {
                            AttemptFailure::transport(
                                attempt,
                                crate::TransportError::new("response_decode", message),
                            )
                        },
                    )?
                };
                Ok((response, notify_observers.then_some(metadata)))
            }
        },
    )
    .await;
    match response {
        Ok((response, metadata)) => {
            let observation = ResponseObservationContext {
                model: &request.model.model_ref,
                api: &request.model.api,
                endpoint: &url,
            };
            if let Some(metadata) = &metadata {
                for observer in request.response_observers.iter() {
                    if let Err(error) = observer.on_response(observation, metadata).await {
                        return terminal_image_failure_at(
                            &request.model,
                            &cancellation,
                            false,
                            redact_image_error(&error.to_string(), &request),
                            timestamp,
                        );
                    }
                }
            }
            match project_openrouter_images_response_at(&request.model, &response, timestamp) {
                Ok(result) => result,
                Err(failure) => terminalize_image_result(
                    *failure.partial,
                    &cancellation,
                    false,
                    redact_image_error(&failure.message, &request),
                ),
            }
        }
        Err(error) => {
            let explicitly_cancelled = matches!(error, AttemptFailure::Cancelled);
            let message = if cancellation.is_cancelled() || explicitly_cancelled {
                "Request aborted".to_owned()
            } else {
                redact_image_error(&error.to_string(), &request)
            };
            terminal_image_failure_at(
                &request.model,
                &cancellation,
                explicitly_cancelled,
                message,
                timestamp,
            )
        }
    }
}

async fn execute_local_openrouter_images(
    transport: Rc<dyn LocalHttpTransport>,
    request: LocalResolvedImageRequest,
    cancellation: CancellationToken,
) -> AssistantImages {
    let timestamp = crate::images::image_timestamp_now();
    if !has_openrouter_api_key(request.api_key.as_ref()) {
        return terminal_image_failure_at(
            &request.model,
            &cancellation,
            false,
            format!(
                "No API key for provider: {}",
                request.model.model_ref.provider
            ),
            timestamp,
        );
    }
    let body = match encode_openrouter_images_request(&request.model, &request.context) {
        Ok(body) => body,
        Err(error) => {
            return terminal_image_failure_at(
                &request.model,
                &cancellation,
                false,
                error.to_string(),
                timestamp,
            );
        }
    };
    let url = openrouter_chat_completions_url(&request.endpoint);
    let mut headers = request.headers.clone();
    if let Err(error) = seed_openrouter_sdk_authorization(
        &mut headers,
        request.api_key.as_ref().expect("API key checked above"),
    ) {
        return terminal_image_failure_at(&request.model, &cancellation, false, error, timestamp);
    }
    headers
        .entry(header::ACCEPT)
        .or_insert(http::HeaderValue::from_static("application/json"));
    headers
        .entry(header::CONTENT_TYPE)
        .or_insert(http::HeaderValue::from_static("application/json"));
    let payload_context = crate::ErasedPayloadContext {
        model: &request.model.model_ref,
        api: &request.model.api,
        endpoint: &url,
        headers: &headers,
    };
    let mut payload = ProviderPayload::json(body);
    for transform in request.payload_transforms.iter() {
        match transform.transform(payload_context, &mut payload).await {
            Ok(PayloadTransformDisposition::Continue) => {}
            Ok(PayloadTransformDisposition::Replace(replacement)) => payload = replacement,
            Err(error) => {
                return terminal_image_failure_at(
                    &request.model,
                    &cancellation,
                    false,
                    redact_local_image_error(&error.to_string(), &request),
                    timestamp,
                );
            }
        }
    }
    let method = payload.method.clone();
    let body = match payload.encode_body() {
        Ok(body) => body,
        Err(error) => {
            return terminal_image_failure_at(
                &request.model,
                &cancellation,
                false,
                redact_local_image_error(&error.to_string(), &request),
                timestamp,
            );
        }
    };
    let frozen = HttpRequest {
        method,
        url: url.clone(),
        headers,
        auth_headers: request.auth_headers.clone(),
        session_id: None,
        body,
        timeout: request.timeout,
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    };
    let classifier = LocalOpenRouterImagesRetryClassifier(Rc::clone(&request.retry_classifier));
    let response = establish_local_openrouter_images_with_retry(
        &request.retry_policy,
        &classifier,
        &cancellation,
        |attempt| {
            let mut attempt_request = frozen.clone();
            let invariant_headers = frozen.auth_headers.clone();
            let transport = Rc::clone(&transport);
            let middleware = Rc::clone(&request.attempt_middleware);
            let cancellation = cancellation.clone();
            async move {
                attempt_request.attempt = attempt;
                for item in middleware.iter() {
                    item.before_attempt(attempt, &mut attempt_request)
                        .await
                        .map_err(|source| AttemptFailure::Middleware { attempt, source })?;
                }
                attempt_request.auth_headers = invariant_headers;
                let mut response = execute_local_http_attempt(
                    transport.as_ref(),
                    attempt_request,
                    cancellation.clone(),
                )
                .await?;
                let metadata = ProviderResponseMetadata {
                    attempt,
                    status: response.status,
                    headers: response.headers.clone(),
                    request_id: request_id_from_headers(&response.headers),
                };
                let notify_observers = response.notify_observers;
                if !(200..300).contains(&response.status) {
                    return Err(
                        send_local_image_http_failure(attempt, response, &cancellation).await,
                    );
                }
                let response = if let Some(response) =
                    openrouter_images_sdk_bodyless_response(response.status, &response.headers)
                {
                    response
                } else {
                    let bytes = collect_local_body(&mut response, &cancellation)
                        .await
                        .map_err(|message| {
                            if cancellation.is_cancelled() {
                                AttemptFailure::Cancelled
                            } else {
                                AttemptFailure::transport(
                                    attempt,
                                    crate::TransportError::new("response_body", message),
                                )
                            }
                        })?;
                    parse_openrouter_images_sdk_response(&response.headers, bytes).map_err(
                        |message| {
                            AttemptFailure::transport(
                                attempt,
                                crate::TransportError::new("response_decode", message),
                            )
                        },
                    )?
                };
                Ok((response, notify_observers.then_some(metadata)))
            }
        },
    )
    .await;
    match response {
        Ok((response, metadata)) => {
            let observation = ResponseObservationContext {
                model: &request.model.model_ref,
                api: &request.model.api,
                endpoint: &url,
            };
            if let Some(metadata) = &metadata {
                for observer in request.response_observers.iter() {
                    if let Err(error) = observer.on_response(observation, metadata).await {
                        return terminal_image_failure_at(
                            &request.model,
                            &cancellation,
                            false,
                            redact_local_image_error(&error.to_string(), &request),
                            timestamp,
                        );
                    }
                }
            }
            match project_openrouter_images_response_at(&request.model, &response, timestamp) {
                Ok(result) => result,
                Err(failure) => terminalize_image_result(
                    *failure.partial,
                    &cancellation,
                    false,
                    redact_local_image_error(&failure.message, &request),
                ),
            }
        }
        Err(error) => {
            let explicitly_cancelled = matches!(error, AttemptFailure::Cancelled);
            let message = if cancellation.is_cancelled() || explicitly_cancelled {
                "Request aborted".to_owned()
            } else {
                redact_local_image_error(&error.to_string(), &request)
            };
            terminal_image_failure_at(
                &request.model,
                &cancellation,
                explicitly_cancelled,
                message,
                timestamp,
            )
        }
    }
}

async fn establish_openrouter_images_with_retry<T, F, Fut>(
    policy: &RetryPolicy,
    classifier: &dyn RetryClassifier,
    cancellation: &CancellationToken,
    mut attempt: F,
) -> Result<T, AttemptFailure>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, AttemptFailure>>,
{
    let mut retry_index = 0;
    loop {
        // Calling the closure mirrors Pi entering the SDK request operation.
        // An already-aborted signal is then rejected by the SDK before its
        // injected fetch is polled.
        let attempt = attempt(retry_index);
        if cancellation.is_cancelled() {
            return Err(AttemptFailure::Cancelled);
        }
        let error = match poll_attempt_before_cancellation(attempt, cancellation).await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if cancellation.is_cancelled() {
            return Err(AttemptFailure::Cancelled);
        }
        if retry_index >= policy.max_retries {
            return Err(classifier.normalize_terminal(error));
        }
        let delay = match classifier.classify(&error, policy) {
            RetryDecision::DoNotRetry => return Err(classifier.normalize_terminal(error)),
            RetryDecision::RetryAfter(delay) => delay,
            RetryDecision::RejectServerDelay { requested, maximum } => {
                return Err(AttemptFailure::RetryDelayTooLong {
                    requested,
                    maximum,
                    source: Box::new(classifier.normalize_terminal(error)),
                });
            }
        };
        cancellable_openrouter_images_sleep(delay, cancellation).await?;
        retry_index += 1;
    }
}

async fn establish_local_openrouter_images_with_retry<T, F, Fut>(
    policy: &RetryPolicy,
    classifier: &dyn LocalRetryClassifier,
    cancellation: &CancellationToken,
    mut attempt: F,
) -> Result<T, AttemptFailure>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, AttemptFailure>>,
{
    let mut retry_index = 0;
    loop {
        let attempt = attempt(retry_index);
        if cancellation.is_cancelled() {
            return Err(AttemptFailure::Cancelled);
        }
        let error = match poll_attempt_before_cancellation(attempt, cancellation).await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if cancellation.is_cancelled() {
            return Err(AttemptFailure::Cancelled);
        }
        if retry_index >= policy.max_retries {
            return Err(classifier.normalize_terminal(error));
        }
        let delay = match classifier.classify(&error, policy) {
            RetryDecision::DoNotRetry => return Err(classifier.normalize_terminal(error)),
            RetryDecision::RetryAfter(delay) => delay,
            RetryDecision::RejectServerDelay { requested, maximum } => {
                return Err(AttemptFailure::RetryDelayTooLong {
                    requested,
                    maximum,
                    source: Box::new(classifier.normalize_terminal(error)),
                });
            }
        };
        cancellable_openrouter_images_sleep(delay, cancellation).await?;
        retry_index += 1;
    }
}

async fn poll_attempt_before_cancellation<T>(
    future: impl std::future::Future<Output = Result<T, AttemptFailure>>,
    cancellation: &CancellationToken,
) -> Result<T, AttemptFailure> {
    // Once the SDK has admitted an attempt, keep the request-versus-abort race
    // left-biased so an already-ready response has the same Promise ordering as
    // Pi. The pre-admission already-aborted case is handled by the caller.
    let future = Box::pin(future);
    let cancelled = Box::pin(cancellation.cancelled());
    match futures_util::future::select(future, cancelled).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(AttemptFailure::Cancelled),
    }
}

async fn cancellable_openrouter_images_sleep(
    delay: std::time::Duration,
    cancellation: &CancellationToken,
) -> Result<(), AttemptFailure> {
    if cancellation.is_cancelled() {
        return Err(AttemptFailure::Cancelled);
    }
    let timer = Box::pin(futures_timer::Delay::new(delay));
    let cancelled = Box::pin(cancellation.cancelled());
    match futures_util::future::select(timer, cancelled).await {
        Either::Left(((), _)) => Ok(()),
        Either::Right(((), _)) => Err(AttemptFailure::Cancelled),
    }
}

fn terminal_image_failure_at(
    model: &ImageModelDescriptor,
    cancellation: &CancellationToken,
    explicitly_cancelled: bool,
    message: impl Into<String>,
    timestamp: crate::Timestamp,
) -> AssistantImages {
    let reason = if cancellation.is_cancelled() || explicitly_cancelled {
        ImageGenerationStopReason::Aborted
    } else {
        ImageGenerationStopReason::Error
    };
    image_failure_at(model, reason, message, timestamp)
}

fn image_failure_at(
    model: &ImageModelDescriptor,
    reason: ImageGenerationStopReason,
    message: impl Into<String>,
    timestamp: crate::Timestamp,
) -> AssistantImages {
    let mut result = AssistantImages::failure(&model.model_ref, model.api.clone(), reason, message);
    result.timestamp = timestamp;
    result
}

fn terminalize_image_result(
    mut result: AssistantImages,
    cancellation: &CancellationToken,
    explicitly_cancelled: bool,
    message: impl Into<String>,
) -> AssistantImages {
    result.stop_reason = if cancellation.is_cancelled() || explicitly_cancelled {
        ImageGenerationStopReason::Aborted
    } else {
        ImageGenerationStopReason::Error
    };
    result.error_message = Some(message.into());
    result
}

fn has_openrouter_api_key(api_key: Option<&crate::SecretString>) -> bool {
    api_key.is_some_and(|key| !key.expose_secret().is_empty())
}

fn seed_openrouter_sdk_authorization(
    headers: &mut http::HeaderMap,
    api_key: &crate::SecretString,
) -> Result<(), String> {
    if headers.contains_key(header::AUTHORIZATION) {
        return Ok(());
    }
    let mut value = http::HeaderValue::from_str(&format!("Bearer {}", api_key.expose_secret()))
        .map_err(|_| "invalid OpenRouter API key for Authorization header".to_owned())?;
    value.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, value);
    Ok(())
}

fn openrouter_chat_completions_url(base: &url::Url) -> url::Url {
    let mut url = base.clone();
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/chat/completions") {
        url.set_path(&format!("{path}/chat/completions"));
    }
    url
}

async fn execute_http_attempt(
    transport: &dyn HttpTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) -> Result<HttpResponse, AttemptFailure> {
    let attempt = request.attempt;
    let timeout = request.timeout;
    let child = cancellation.child();
    let execution = Box::pin(transport.execute(request, child.clone()));
    let Some(timeout) = timeout else {
        return execution
            .await
            .map_err(|source| AttemptFailure::transport(attempt, source));
    };
    let timer = Box::pin(futures_timer::Delay::new(timeout));
    match futures_util::future::select(execution, timer).await {
        Either::Left((result, _)) => {
            result.map_err(|source| AttemptFailure::transport(attempt, source))
        }
        Either::Right(((), _)) => {
            child.cancel();
            Err(AttemptFailure::Timeout { attempt, timeout })
        }
    }
}

async fn execute_local_http_attempt(
    transport: &dyn LocalHttpTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) -> Result<LocalHttpResponse, AttemptFailure> {
    let attempt = request.attempt;
    let timeout = request.timeout;
    let child = cancellation.child();
    let execution = Box::pin(transport.execute(request, child.clone()));
    let Some(timeout) = timeout else {
        return execution
            .await
            .map_err(|source| AttemptFailure::transport(attempt, source));
    };
    let timer = Box::pin(futures_timer::Delay::new(timeout));
    match futures_util::future::select(execution, timer).await {
        Either::Left((result, _)) => {
            result.map_err(|source| AttemptFailure::transport(attempt, source))
        }
        Either::Right(((), _)) => {
            child.cancel();
            Err(AttemptFailure::Timeout { attempt, timeout })
        }
    }
}

async fn send_image_http_failure(
    attempt: u32,
    mut response: HttpResponse,
    cancellation: &CancellationToken,
) -> AttemptFailure {
    let bytes = match collect_send_body(&mut response, cancellation).await {
        Ok(bytes) => bytes,
        Err(_) if cancellation.is_cancelled() => return AttemptFailure::Cancelled,
        Err(message) => {
            return AttemptFailure::http_at(
                attempt,
                response.status,
                response.headers,
                SystemTime::now(),
                message,
            );
        }
    };
    AttemptFailure::http_at(
        attempt,
        response.status,
        response.headers,
        SystemTime::now(),
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn send_local_image_http_failure(
    attempt: u32,
    mut response: LocalHttpResponse,
    cancellation: &CancellationToken,
) -> AttemptFailure {
    let bytes = match collect_local_body(&mut response, cancellation).await {
        Ok(bytes) => bytes,
        Err(_) if cancellation.is_cancelled() => return AttemptFailure::Cancelled,
        Err(message) => {
            return AttemptFailure::http_at(
                attempt,
                response.status,
                response.headers,
                SystemTime::now(),
                message,
            );
        }
    };
    AttemptFailure::http_at(
        attempt,
        response.status,
        response.headers,
        SystemTime::now(),
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn collect_send_body(
    response: &mut HttpResponse,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, String> {
    collect_body(&mut response.body, cancellation).await
}

async fn collect_local_body(
    response: &mut LocalHttpResponse,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, String> {
    collect_body(&mut response.body, cancellation).await
}

async fn collect_body<S>(body: &mut S, cancellation: &CancellationToken) -> Result<Vec<u8>, String>
where
    S: futures_core::Stream<Item = Result<Vec<u8>, crate::TransportError>> + Unpin + ?Sized,
{
    let mut bytes = Vec::new();
    loop {
        let chunk = match futures_util::future::select(
            Box::pin(body.next()),
            Box::pin(cancellation.cancelled()),
        )
        .await
        {
            Either::Left((chunk, _)) => chunk,
            Either::Right(((), _)) => return Err("Request aborted".into()),
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|error| error.to_string())?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn redact_image_error(message: &str, request: &ResolvedImageRequest) -> String {
    redact_image_error_values(
        message,
        &request.auth_headers,
        &request.headers,
        request.environment.values(),
        request.api_key.as_ref(),
    )
}

fn redact_local_image_error(message: &str, request: &LocalResolvedImageRequest) -> String {
    redact_image_error_values(
        message,
        &request.auth_headers,
        &request.headers,
        request.environment.values(),
        request.api_key.as_ref(),
    )
}

fn redact_image_error_values<'a>(
    message: &str,
    auth_headers: &http::HeaderMap,
    headers: &http::HeaderMap,
    environment: impl Iterator<Item = &'a String>,
    api_key: Option<&crate::SecretString>,
) -> String {
    let mut secrets = Vec::new();
    for (name, value) in auth_headers.iter().chain(
        headers
            .iter()
            .filter(|(name, _)| image_sensitive_header_name(name.as_str())),
    ) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        secrets.push(value.to_owned());
        if name == header::AUTHORIZATION
            && let Some(token) = value
                .strip_prefix("Bearer ")
                .filter(|token| !token.is_empty())
        {
            secrets.push(token.to_owned());
        }
    }
    secrets.extend(environment.filter(|value| !value.is_empty()).cloned());
    if let Some(api_key) = api_key {
        secrets.push(api_key.expose_secret().to_owned());
    }
    let secret_refs = secrets.iter().map(String::as_str).collect::<Vec<_>>();
    crate::sanitization::redact_public_text(message.to_owned(), &secret_refs)
}

fn image_sensitive_header_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cf-aig-authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "api-key"
            | "cookie"
            | "set-cookie"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CacheWriteRetentionPricing, HeaderMapSpec, ModelRef, MoneyRate, RequestWidePriceTier,
        TokenPriceRates,
    };
    use url::Url;

    fn model(output: Vec<ImageModality>) -> ImageModelDescriptor {
        ImageModelDescriptor {
            model_ref: ModelRef::new("openrouter", "fixture/image"),
            display_name: "Fixture Image".into(),
            api: crate::OPENROUTER_IMAGES_API_ID.into(),
            base_url: Url::parse("https://openrouter.ai/api/v1").expect("valid fixture URL"),
            input: vec![ImageModality::Text, ImageModality::Image],
            output,
            pricing: crate::ModelPricing {
                default: TokenPriceRates {
                    input: MoneyRate::new(1_000_000),
                    output: MoneyRate::new(2_000_000),
                    cache_read: MoneyRate::new(0),
                    cache_write: MoneyRate::new(0),
                },
                request_wide_tiers: Vec::<RequestWidePriceTier>::new(),
                cache_write_retention: CacheWriteRetentionPricing::default(),
            },
            headers: HeaderMapSpec::new(),
        }
    }

    #[test]
    fn openrouter_images_lowering_preserves_modalities_and_data_urls() {
        // Pi basis: packages/ai/src/api/openrouter-images.ts buildParams and
        // packages/ai/test/openrouter-images.test.ts request-body scenarios.
        let bytes = encode_openrouter_images_request(
            &model(vec![ImageModality::Text, ImageModality::Image]),
            &ImageGenerationContext {
                input: vec![
                    ImageGenerationContent::text("draw it"),
                    ImageGenerationContent::image("AAAA", "image/png"),
                ],
            },
        )
        .expect("encoding succeeds");
        assert_eq!(
            String::from_utf8(bytes).expect("JSON is UTF-8"),
            r#"{"model":"fixture/image","messages":[{"role":"user","content":[{"type":"text","text":"draw it"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}],"stream":false,"modalities":["image","text"]}"#
        );
    }

    #[test]
    fn openrouter_images_decoder_extracts_text_and_valid_data_urls() {
        // Pi basis: packages/ai/src/api/openrouter-images.ts first-choice
        // decoding and packages/ai/test/openrouter-images.test.ts response scenarios.
        let response = br#"{"id":"generation-1","usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20},"choices":[{"message":{"content":"caption","images":[{"image_url":{"url":"data:image/png;base64,AAAA"}},{"image_url":"https://example.invalid/image.png"},{"image_url":{"url":"data:image/png;base64,"}}]}}]}"#;
        let decoded =
            decode_openrouter_images_response(&model(vec![ImageModality::Image]), response)
                .expect("decoding succeeds");
        assert_eq!(decoded.response_id.as_deref(), Some("generation-1"));
        assert_eq!(
            decoded.output,
            vec![
                ImageGenerationContent::text("caption"),
                ImageGenerationContent::image("AAAA", "image/png"),
            ]
        );
        assert_eq!(decoded.usage.expect("usage").total_tokens(), 20);
    }

    #[test]
    fn openrouter_images_decoder_preserves_null_usage_and_rejects_malformed_shapes() {
        // Pi basis: packages/ai/src/api/openrouter-images.ts initializes output
        // before the request, omits falsy usage, and requires choices/message
        // through ordinary property access in its OpenAI response projection.
        let model = model(vec![ImageModality::Image]);
        let empty = decode_openrouter_images_response(
            &model,
            br#"{"id":"empty","usage":null,"choices":[]}"#,
        )
        .expect("an empty choices array is a successful empty output");
        assert!(empty.usage.is_none());
        assert!(empty.cost.is_none());
        assert!(empty.output.is_empty());

        let null_choice = decode_openrouter_images_response(
            &model,
            br#"{"id":"null-choice","usage":{"prompt_tokens":3,"completion_tokens":2},"choices":[null]}"#,
        )
        .expect("Pi treats a null first choice as an empty successful result");
        assert_eq!(null_choice.response_id.as_deref(), Some("null-choice"));
        assert_eq!(null_choice.usage.expect("usage retained").total_tokens(), 5);
        assert!(null_choice.output.is_empty());

        for malformed in [
            br#"{}"#.as_slice(),
            br#"{"choices":null}"#.as_slice(),
            br#"{"choices":[{}]}"#.as_slice(),
            br#"{"choices":[{"message":{"images":{}}}]}"#.as_slice(),
            br#"{"choices":[{"message":{"images":[null]}}]}"#.as_slice(),
        ] {
            assert!(
                decode_openrouter_images_response(&model, malformed).is_err(),
                "malformed response was accepted: {}",
                String::from_utf8_lossy(malformed)
            );
        }
    }

    #[test]
    fn openrouter_images_json_replacement_decodes_invalid_utf8_pi_exact() {
        // Pi basis: packages/ai/src/api/openrouter-images.ts uses OpenAI SDK
        // `.withResponse()`, whose `Response.json()` UTF-8 decoding replaces
        // malformed input before JSON parsing.
        let mut response = br#"{"id":""#.to_vec();
        response.push(0xff);
        response.extend_from_slice(br#"","choices":[]}"#);
        let decoded =
            decode_openrouter_images_response(&model(vec![ImageModality::Image]), &response)
                .expect("replacement-decoded JSON succeeds");
        assert_eq!(decoded.response_id.as_deref(), Some("�"));
    }
}
