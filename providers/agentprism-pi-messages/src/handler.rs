//! Erased handler and raw HTTP adapters for pi-messages.

#![allow(
    clippy::result_large_err,
    reason = "ErasedApiHandler requires the architecture-specified AiError by value"
)]

use crate::{
    PiMessages, PiMessagesCompat, PiMessagesDecodeContext, PiMessagesOptions,
    PiMessagesSimplePatch, PiMessagesSseDecoder,
};
use agentprism_ai::{
    ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION, AiError, AiErrorKind, ApiExecutionContext,
    ApiFamily, ApiId, ApiModelConfig, AssistantMessageDiagnostic, AssistantStream,
    CONTEXT_SAFETY_TOKENS, CacheRetention, CancellationToken, ChatApi, Context,
    DiagnosticErrorCode, DiagnosticErrorInfo, EncodeContext, ErasedApiFullOptions,
    ErasedApiHandler, ErasedApiOptionsPatch, HttpBody, HttpChatApi, HttpRequest, HttpResponse,
    HttpTransport, LocalApiExecutionContext, LocalAssistantStream, LocalBoxFuture, LocalChatApi,
    LocalErasedApiHandler, LocalHttpBody, LocalHttpChatApi, LocalHttpResponse, LocalHttpTransport,
    LocalProviderResponseStream, MAX_PROVIDER_ERROR_BODY_BYTES, MessageId, MiddlewareError,
    ModelRef, OrderedJsonWriter, ProviderPayload, ProviderResponseStream, SendBoxFuture,
    SimpleGenerationOptions, SimpleLoweringContext, Timestamp, TransportError,
    TypedModelDescriptor, estimate_context_tokens,
};
use futures_util::{FutureExt, StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, header};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Native pi-messages erased API-family implementation.
#[derive(Clone, Debug)]
pub struct PiMessagesHandler {
    api: ApiId,
}

impl Default for PiMessagesHandler {
    fn default() -> Self {
        Self {
            api: ApiId::new(PiMessages::API_ID),
        }
    }
}

impl ErasedApiHandler for PiMessagesHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &agentprism_ai::ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        lower_and_encode(
            model,
            context,
            simple,
            patch,
            execution.endpoint,
            execution.auth_headers,
        )
    }

    fn encode_full(
        &self,
        model: &agentprism_ai::ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        encode_full(
            model,
            context,
            options,
            execution.endpoint,
            execution.auth_headers,
        )
    }

    fn decode_stream(
        &self,
        mut response: ProviderResponseStream,
        execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream {
        let mut decoder = PiMessagesSseDecoder::new(decode_context(execution.model));
        for mut diagnostic in std::mem::take(&mut response.diagnostics) {
            enrich_response_failure_diagnostic(&mut diagnostic, &execution.model.common.model_ref);
            decoder.add_diagnostic(diagnostic);
        }
        let pending = decoder.take_events().into();
        AssistantStream::new(stream::unfold(
            SendDecodeState {
                body: response.body,
                decoder,
                model: execution.model.common.model_ref.clone(),
                cancellation: execution.cancellation.clone(),
                pending,
                done: false,
            },
            next_send_event,
        ))
    }
}

impl LocalErasedApiHandler for PiMessagesHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &agentprism_ai::ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        lower_and_encode(
            model,
            context,
            simple,
            patch,
            execution.endpoint,
            execution.auth_headers,
        )
    }

    fn encode_full(
        &self,
        model: &agentprism_ai::ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        encode_full(
            model,
            context,
            options,
            execution.endpoint,
            execution.auth_headers,
        )
    }

    fn decode_stream(
        &self,
        mut response: LocalProviderResponseStream,
        execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream {
        let mut decoder = PiMessagesSseDecoder::new(decode_context(execution.model));
        for mut diagnostic in std::mem::take(&mut response.diagnostics) {
            enrich_response_failure_diagnostic(&mut diagnostic, &execution.model.common.model_ref);
            decoder.add_diagnostic(diagnostic);
        }
        let pending = decoder.take_events().into();
        LocalAssistantStream::new(stream::unfold(
            LocalDecodeState {
                body: response.body,
                decoder,
                model: execution.model.common.model_ref.clone(),
                cancellation: execution.cancellation.clone(),
                pending,
                done: false,
            },
            next_local_event,
        ))
    }
}

struct SendDecodeState {
    body: HttpBody,
    decoder: PiMessagesSseDecoder,
    model: ModelRef,
    cancellation: CancellationToken,
    pending: VecDeque<agentprism_ai::AssistantEvent>,
    done: bool,
}

struct LocalDecodeState {
    body: LocalHttpBody,
    decoder: PiMessagesSseDecoder,
    model: ModelRef,
    cancellation: CancellationToken,
    pending: VecDeque<agentprism_ai::AssistantEvent>,
    done: bool,
}

enum BodyPoll {
    Cancelled,
    Body(Option<Result<Vec<u8>, agentprism_ai::TransportError>>),
}

async fn next_send_event(
    mut state: SendDecodeState,
) -> Option<(agentprism_ai::AssistantEvent, SendDecodeState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Some((event, state));
        }
        if state.done {
            return None;
        }
        match next_send_body(&mut state.body, &state.cancellation).await {
            BodyPoll::Cancelled => {
                state
                    .pending
                    .extend(state.decoder.cancel("Request was aborted"));
                state.done = true;
            }
            BodyPoll::Body(Some(Ok(chunk))) => {
                state.pending.extend(state.decoder.push(&chunk));
                state.done = state.decoder.is_terminated();
            }
            BodyPoll::Body(Some(Err(mut error))) => {
                for mut diagnostic in std::mem::take(&mut error.diagnostics) {
                    enrich_response_failure_diagnostic(&mut diagnostic, &state.model);
                    state.decoder.add_diagnostic(diagnostic);
                }
                state
                    .pending
                    .extend(state.decoder.fail_transport_error(error));
                state.done = true;
            }
            BodyPoll::Body(None) => {
                state.pending.extend(state.decoder.finish());
                state.done = true;
            }
        }
    }
}

async fn next_local_event(
    mut state: LocalDecodeState,
) -> Option<(agentprism_ai::AssistantEvent, LocalDecodeState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Some((event, state));
        }
        if state.done {
            return None;
        }
        match next_local_body(&mut state.body, &state.cancellation).await {
            BodyPoll::Cancelled => {
                state
                    .pending
                    .extend(state.decoder.cancel("Request was aborted"));
                state.done = true;
            }
            BodyPoll::Body(Some(Ok(chunk))) => {
                state.pending.extend(state.decoder.push(&chunk));
                state.done = state.decoder.is_terminated();
            }
            BodyPoll::Body(Some(Err(mut error))) => {
                for mut diagnostic in std::mem::take(&mut error.diagnostics) {
                    enrich_response_failure_diagnostic(&mut diagnostic, &state.model);
                    state.decoder.add_diagnostic(diagnostic);
                }
                state
                    .pending
                    .extend(state.decoder.fail_transport_error(error));
                state.done = true;
            }
            BodyPoll::Body(None) => {
                state.pending.extend(state.decoder.finish());
                state.done = true;
            }
        }
    }
}

async fn next_send_body(body: &mut HttpBody, cancellation: &CancellationToken) -> BodyPoll {
    if cancellation.is_cancelled() {
        return BodyPoll::Cancelled;
    }
    let cancelled = cancellation.cancelled().fuse();
    let next = body.next().fuse();
    futures_util::pin_mut!(cancelled, next);
    futures_util::select_biased! {
        _ = cancelled => BodyPoll::Cancelled,
        item = next => BodyPoll::Body(item),
    }
}

async fn next_local_body(body: &mut LocalHttpBody, cancellation: &CancellationToken) -> BodyPoll {
    if cancellation.is_cancelled() {
        return BodyPoll::Cancelled;
    }
    let cancelled = cancellation.cancelled().fuse();
    let next = body.next().fuse();
    futures_util::pin_mut!(cancelled, next);
    futures_util::select_biased! {
        _ = cancelled => BodyPoll::Cancelled,
        item = next => BodyPoll::Body(item),
    }
}

fn lower_and_encode(
    model: &agentprism_ai::ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    patch: Option<&ErasedApiOptionsPatch>,
    endpoint: &Url,
    auth_headers: &HeaderMap,
) -> Result<ProviderPayload, AiError> {
    let typed = typed_model(model)?;
    let compat = PiMessagesCompat;
    let estimate = estimate_context_tokens(context)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let available = model
        .common
        .limits
        .context_window
        .saturating_sub(estimate.tokens)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    let mut options = PiMessages::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &compat,
            effective_base_url: endpoint,
            estimated_input_tokens: estimate.tokens,
            available_context_tokens: available,
        },
        simple,
        &parse_patch(model, patch)?,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    apply_environment_cache_retention(&mut options, auth_headers);
    encode_options(model, context, endpoint, typed, &compat, &options)
}

fn encode_full(
    model: &agentprism_ai::ModelDescriptor,
    context: &Context,
    options: &ErasedApiFullOptions,
    endpoint: &Url,
    auth_headers: &HeaderMap,
) -> Result<ProviderPayload, AiError> {
    let typed = typed_model(model)?;
    let mut options = options
        .downcast_ref::<PiMessages>()
        .ok_or_else(|| invalid_request(model, "invalid pi-messages options type"))?
        .clone();
    let compat = PiMessagesCompat;
    apply_environment_cache_retention(&mut options, auth_headers);
    encode_options(model, context, endpoint, typed, &compat, &options)
}

/// Private auth-to-API carrier set by the Radius provider. It never becomes a
/// logical HTTP header.
pub const PI_CACHE_RETENTION_AUTH_HEADER: &str = "x-pi-cache-retention-environment";

fn apply_environment_cache_retention(options: &mut PiMessagesOptions, auth_headers: &HeaderMap) {
    if options.cache_retention.is_none()
        && auth_headers
            .get(PI_CACHE_RETENTION_AUTH_HEADER)
            .and_then(|value| value.to_str().ok())
            == Some("long")
    {
        options.cache_retention = Some(CacheRetention::Long);
    }
}

fn typed_model(
    model: &agentprism_ai::ModelDescriptor,
) -> Result<TypedModelDescriptor<PiMessages>, AiError> {
    let ApiModelConfig::Custom(config) = &model.api else {
        return Err(invalid_request(model, "model does not use pi-messages"));
    };
    if config.api.as_str() != PiMessages::API_ID {
        return Err(invalid_request(model, "model does not use pi-messages"));
    }
    Ok(TypedModelDescriptor {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    })
}

fn encode_options(
    model: &agentprism_ai::ModelDescriptor,
    context: &Context,
    endpoint: &Url,
    typed: TypedModelDescriptor<PiMessages>,
    compat: &PiMessagesCompat,
    options: &PiMessagesOptions,
) -> Result<ProviderPayload, AiError> {
    let wire = PiMessages::encode(
        EncodeContext {
            model: &typed,
            context,
            compat,
            effective_base_url: endpoint,
        },
        options,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    Ok(
        ProviderPayload::typed::<PiMessages, _>(Method::POST, typed, wire, |request| {
            OrderedJsonWriter::to_vec(&request.clone().into()).map_err(|error| {
                MiddlewareError::new(
                    "provider_payload_encode",
                    format!("failed to encode pi-messages payload: {error}"),
                )
            })
        })
        .with_transport_session_id(options.debug.then(|| "pi-messages-debug=1".into())),
    )
}

fn parse_patch(
    model: &agentprism_ai::ModelDescriptor,
    patch: Option<&ErasedApiOptionsPatch>,
) -> Result<PiMessagesSimplePatch, AiError> {
    let Some(patch) = patch else {
        return Ok(PiMessagesSimplePatch::default());
    };
    if patch.schema_version != 1 {
        return Err(invalid_request(
            model,
            format!(
                "unsupported pi-messages options schema version {}",
                patch.schema_version
            ),
        ));
    }
    serde_json::from_str(patch.value.get())
        .map_err(|error| invalid_request(model, format!("invalid API options patch: {error}")))
}

fn decode_context(model: &agentprism_ai::ModelDescriptor) -> PiMessagesDecodeContext {
    PiMessagesDecodeContext {
        message_id: MessageId::new(format!(
            "pi-messages-message-{}",
            NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
        )),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        timestamp: now_timestamp(),
    }
}

fn now_timestamp() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
}

fn invalid_request(model: &agentprism_ai::ModelDescriptor, message: impl Into<String>) -> AiError {
    AiError::new(AiErrorKind::InvalidRequest, message).with_model(model.common.model_ref.clone())
}

/// Creates the Send pi-messages API over an injected raw transport.
pub fn pi_messages_api(transport: Arc<dyn HttpTransport>) -> Arc<dyn ChatApi> {
    Arc::new(HttpChatApi::new(
        Arc::new(PiMessagesHandler::default()),
        Arc::new(PiMessagesTransport { inner: transport }),
    ))
}

/// Creates the local-executor pi-messages API.
pub fn local_pi_messages_api(transport: Rc<dyn LocalHttpTransport>) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalHttpChatApi::new(
        Rc::new(PiMessagesHandler::default()),
        Rc::new(LocalPiMessagesTransport { inner: transport }),
    ))
}

#[derive(Clone)]
struct PiMessagesTransport {
    inner: Arc<dyn HttpTransport>,
}

impl HttpTransport for PiMessagesTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, agentprism_ai::TransportError>> {
        request.url = pi_messages_url(
            &request.url,
            request.session_id.as_deref() == Some("pi-messages-debug=1"),
        );
        ensure_headers(&mut request.headers);
        let url = request.url.clone();
        let secret_values = request_secret_values(&request);
        Box::pin(async move {
            let response = self.inner.execute(request, cancellation.clone()).await?;
            normalize_send_response(response, url, secret_values, cancellation)
        })
    }
}

#[derive(Clone)]
struct LocalPiMessagesTransport {
    inner: Rc<dyn LocalHttpTransport>,
}

impl LocalHttpTransport for LocalPiMessagesTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, agentprism_ai::TransportError>> {
        request.url = pi_messages_url(
            &request.url,
            request.session_id.as_deref() == Some("pi-messages-debug=1"),
        );
        ensure_headers(&mut request.headers);
        let url = request.url.clone();
        let secret_values = request_secret_values(&request);
        Box::pin(async move {
            let response = self.inner.execute(request, cancellation.clone()).await?;
            normalize_local_response(response, url, secret_values, cancellation)
        })
    }
}

fn normalize_send_response(
    mut response: HttpResponse,
    url: Url,
    secret_values: Vec<String>,
    cancellation: CancellationToken,
) -> Result<HttpResponse, TransportError> {
    if (200..300).contains(&response.status) {
        return Ok(response);
    }
    let status = response.status;
    let headers = response.headers.clone();
    let mut body = std::mem::replace(&mut response.body, Box::pin(stream::empty()));
    response.decode_non_success = true;
    response.body = Box::pin(stream::once(async move {
        let bytes = read_send_failure_body(&mut body, &cancellation).await?;
        let failure = response_failure(status, &headers, &url, &bytes, &secret_values);
        Err(failure.error.with_diagnostic(failure.diagnostic))
    }));
    Ok(response)
}

fn normalize_local_response(
    mut response: LocalHttpResponse,
    url: Url,
    secret_values: Vec<String>,
    cancellation: CancellationToken,
) -> Result<LocalHttpResponse, TransportError> {
    if (200..300).contains(&response.status) {
        return Ok(response);
    }
    let status = response.status;
    let headers = response.headers.clone();
    let mut body = std::mem::replace(&mut response.body, Box::pin(stream::empty()));
    response.decode_non_success = true;
    response.body = Box::pin(stream::once(async move {
        let bytes = read_local_failure_body(&mut body, &cancellation).await?;
        let failure = response_failure(status, &headers, &url, &bytes, &secret_values);
        Err(failure.error.with_diagnostic(failure.diagnostic))
    }));
    Ok(response)
}

async fn read_send_failure_body(
    body: &mut HttpBody,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, TransportError> {
    let mut bytes = Vec::new();
    while bytes.len() < MAX_PROVIDER_ERROR_BODY_BYTES {
        cancellation.check().map_err(|_| cancelled_body_error())?;
        let next = body.next().fuse();
        let cancelled = cancellation.cancelled().fuse();
        futures_util::pin_mut!(next, cancelled);
        let chunk = futures_util::select_biased! {
            _ = cancelled => return Err(cancelled_body_error()),
            chunk = next => chunk,
        };
        let Some(chunk) = chunk else { break };
        let chunk = chunk?;
        let remaining = MAX_PROVIDER_ERROR_BODY_BYTES - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(bytes)
}

async fn read_local_failure_body(
    body: &mut LocalHttpBody,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, TransportError> {
    let mut bytes = Vec::new();
    while bytes.len() < MAX_PROVIDER_ERROR_BODY_BYTES {
        cancellation.check().map_err(|_| cancelled_body_error())?;
        let next = body.next().fuse();
        let cancelled = cancellation.cancelled().fuse();
        futures_util::pin_mut!(next, cancelled);
        let chunk = futures_util::select_biased! {
            _ = cancelled => return Err(cancelled_body_error()),
            chunk = next => chunk,
        };
        let Some(chunk) = chunk else { break };
        let chunk = chunk?;
        let remaining = MAX_PROVIDER_ERROR_BODY_BYTES - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(bytes)
}

fn cancelled_body_error() -> TransportError {
    TransportError::new("cancelled", "request cancelled while reading response body")
}

struct PiMessagesResponseFailure {
    diagnostic: AssistantMessageDiagnostic,
    error: TransportError,
}

fn response_failure(
    status: u16,
    headers: &HeaderMap,
    url: &Url,
    body: &[u8],
    secret_values: &[String],
) -> PiMessagesResponseFailure {
    let status_text = http::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or_default();
    let body = String::from_utf8_lossy(body).into_owned();
    let parsed = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .and_then(|root| root.get("error").and_then(Value::as_object).cloned());
    let provider_code = parsed
        .as_ref()
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let provider_message = parsed
        .as_ref()
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str);
    let suffix = provider_message.unwrap_or(&body);
    let code_suffix = provider_code
        .as_deref()
        .map(|code| format!(" ({code})"))
        .unwrap_or_default();
    let message = redact_text(
        format!("{status} {status_text}: {suffix}{code_suffix}"),
        secret_values,
    );
    let observed_at = now_timestamp();
    let mut details = BTreeMap::from([
        ("version".into(), Value::from(1)),
        ("url".into(), Value::String(url.to_string())),
        ("status".into(), Value::from(status)),
        ("statusText".into(), Value::String(status_text.into())),
        ("timestampMs".into(), Value::from(observed_at.0)),
    ]);
    if let Some(mut error) = parsed {
        redact_json_object(&mut error, secret_values);
        details.insert("error".into(), Value::Object(error));
    } else {
        details.insert(
            "body".into(),
            Value::String(truncate_diagnostic(&redact_text(body, secret_values))),
        );
    }
    let request_id = request_id(headers);
    PiMessagesResponseFailure {
        diagnostic: AssistantMessageDiagnostic {
            schema_version: ASSISTANT_MESSAGE_DIAGNOSTIC_SCHEMA_VERSION,
            kind: "pi_messages_response_failure".into(),
            timestamp: observed_at,
            error: Some(DiagnosticErrorInfo {
                name: Some("PiMessagesResponseError".into()),
                message: message.clone(),
                stack: None,
                code: provider_code.clone().map(DiagnosticErrorCode::String),
            }),
            details,
        },
        error: TransportError::new("pi_messages_response_failure", message)
            .with_status(status)
            .with_optional_provider_code(provider_code)
            .with_optional_request_id(request_id),
    }
}

trait TransportErrorOptions {
    fn with_optional_provider_code(self, provider_code: Option<String>) -> Self;
    fn with_optional_request_id(self, request_id: Option<String>) -> Self;
}

impl TransportErrorOptions for TransportError {
    fn with_optional_provider_code(mut self, provider_code: Option<String>) -> Self {
        self.provider_code = provider_code;
        self
    }

    fn with_optional_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }
}

fn enrich_response_failure_diagnostic(
    diagnostic: &mut AssistantMessageDiagnostic,
    model: &ModelRef,
) {
    if diagnostic.kind != "pi_messages_response_failure" {
        return;
    }
    diagnostic.details.insert(
        "provider".into(),
        Value::String(model.provider.as_str().into()),
    );
    diagnostic
        .details
        .insert("model".into(), Value::String(model.model.as_str().into()));
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    ["request-id", "x-request-id", "x-amzn-requestid"]
        .into_iter()
        .find_map(|name| headers.get(name)?.to_str().ok().map(str::to_owned))
}

fn request_secret_values(request: &HttpRequest) -> Vec<String> {
    request
        .headers
        .iter()
        .chain(request.auth_headers.iter())
        .filter(|(name, _)| {
            matches!(
                name.as_str().to_ascii_lowercase().as_str(),
                "authorization" | "proxy-authorization" | "x-api-key" | "api-key"
            )
        })
        .filter_map(|(_, value)| value.to_str().ok())
        .flat_map(|value| {
            [
                value.to_owned(),
                value.strip_prefix("Bearer ").unwrap_or(value).to_owned(),
            ]
        })
        .filter(|value| !value.is_empty())
        .collect()
}

fn redact_text(mut value: String, secret_values: &[String]) -> String {
    let mut secret_values = secret_values.iter().collect::<Vec<_>>();
    secret_values.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
    for secret in secret_values {
        value = value.replace(secret, "[REDACTED]");
    }
    value
}

fn redact_json_object(object: &mut Map<String, Value>, secret_values: &[String]) {
    for (key, value) in object {
        if matches!(
            key.to_ascii_lowercase().as_str(),
            "authorization"
                | "api_key"
                | "apikey"
                | "access_token"
                | "refresh_token"
                | "password"
                | "cookie"
        ) {
            *value = Value::String("[REDACTED]".into());
            continue;
        }
        match value {
            Value::String(text) => *text = redact_text(std::mem::take(text), secret_values),
            Value::Object(object) => redact_json_object(object, secret_values),
            Value::Array(values) => {
                for value in values {
                    if let Value::Object(object) = value {
                        redact_json_object(object, secret_values);
                    } else if let Value::String(text) = value {
                        *text = redact_text(std::mem::take(text), secret_values);
                    }
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

fn truncate_diagnostic(value: &str) -> String {
    let mut characters = value.chars();
    let prefix = characters.by_ref().take(8192).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn pi_messages_url(base: &Url, debug: bool) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("{path}/messages"));
    if debug {
        url.query_pairs_mut().append_pair("debug", "1");
    }
    url
}

fn ensure_headers(headers: &mut HeaderMap) {
    headers
        .entry(header::ACCEPT)
        .or_insert(HeaderValue::from_static("text/event-stream"));
    headers
        .entry(header::CONTENT_TYPE)
        .or_insert(HeaderValue::from_static("application/json"));
}
