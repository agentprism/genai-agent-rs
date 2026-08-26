//! Bedrock handler, Smithy build-stage transport, and provider registration.

#![allow(
    clippy::result_large_err,
    reason = "ErasedApiHandler requires the architecture-specified AiError by value"
)]

use crate::{
    BedrockConverseDecoder, BedrockDecodeContext,
    auth::{
        SIGNING_CONFIG_HEADER, bedrock_auth_resolver, local_bedrock_auth_resolver,
        signing_config_from_headers,
    },
    bedrock_models,
};
use agentprism_ai::{
    AiError, AiErrorKind, ApiExecutionContext, ApiFamily, ApiId, ApiModelConfig,
    AssistantMessageDiagnostic, AssistantStream, AuthResolutionOverrides, BedrockConverseStream,
    BedrockHandoff, BedrockOptions, BedrockSimplePatch, CONTEXT_SAFETY_TOKENS, CancellationToken,
    ChatApi, Context, EncodeContext, ErasedApiFullOptions, ErasedApiHandler, ErasedApiOptionsPatch,
    HttpBody, HttpChatApi, HttpRequest, HttpResponse, HttpTransport, LocalApiExecutionContext,
    LocalAssistantStream, LocalBoxFuture, LocalBoxStream, LocalChatApi, LocalErasedApiHandler,
    LocalHttpBody, LocalHttpChatApi, LocalHttpResponse, LocalHttpTransport,
    LocalProviderRegistration, LocalProviderResponseStream, MessageId, MiddlewareError,
    ModelDescriptor, OrderedJsonValue, OrderedJsonWriter, ProviderPayload, ProviderRegistration,
    ProviderRegistrationError, ProviderResponseStream, SendBoxFuture, SendBoxStream,
    SimpleGenerationOptions, SimpleLoweringContext, Timestamp, TransportError,
    TypedModelDescriptor, apply_bedrock_signer_headers, estimate_context_tokens,
    parse_ordered_json, transform_context_for_model, trim_ecmascript,
};
use futures_util::{FutureExt, StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, header};
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Erased handler for the Bedrock Converse Stream API family.
#[derive(Clone, Debug)]
pub struct BedrockConverseHandler {
    api: ApiId,
}

impl Default for BedrockConverseHandler {
    fn default() -> Self {
        Self {
            api: ApiId::new(BedrockConverseStream::API_ID),
        }
    }
}

impl ErasedApiHandler for BedrockConverseHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
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
        model: &ModelDescriptor,
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

    fn apply_full_options_auth_overrides(
        &self,
        model: &ModelDescriptor,
        options: &ErasedApiFullOptions,
        overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        apply_full_options_auth_overrides(model, options, overrides)
    }

    fn decode_stream(
        &self,
        mut response: ProviderResponseStream,
        execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream {
        let mut decoder = BedrockConverseDecoder::new(decode_context(execution.model));
        decoder.observe_response(response.status, &response.headers);
        for diagnostic in std::mem::take(&mut response.diagnostics) {
            decoder.add_diagnostic(diagnostic);
        }
        decode_send_body(response.body, decoder, execution.cancellation.clone())
    }
}

impl LocalErasedApiHandler for BedrockConverseHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
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
        model: &ModelDescriptor,
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

    fn apply_full_options_auth_overrides(
        &self,
        model: &ModelDescriptor,
        options: &ErasedApiFullOptions,
        overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        apply_full_options_auth_overrides(model, options, overrides)
    }

    fn decode_stream(
        &self,
        mut response: LocalProviderResponseStream,
        execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream {
        let mut decoder = BedrockConverseDecoder::new(decode_context(execution.model));
        decoder.observe_response(response.status, &response.headers);
        for diagnostic in std::mem::take(&mut response.diagnostics) {
            decoder.add_diagnostic(diagnostic);
        }
        decode_local_body(response.body, decoder, execution.cancellation.clone())
    }
}

fn apply_full_options_auth_overrides(
    model: &ModelDescriptor,
    options: &ErasedApiFullOptions,
    overrides: &mut AuthResolutionOverrides,
) -> Result<(), AiError> {
    let options = options
        .downcast_ref::<BedrockConverseStream>()
        .ok_or_else(|| invalid_request(model, "invalid bedrock-converse-stream options type"))?;
    if let Some(token) = &options.bearer_token {
        overrides.api_key = Some(token.clone());
    }
    if let Some(profile) = &options.profile {
        overrides
            .environment
            .insert("AWS_PROFILE".to_owned(), profile.clone());
    }
    if let Some(region) = &options.region {
        overrides
            .environment
            .insert("AWS_REGION".to_owned(), region.clone());
    }
    Ok(())
}

fn lower_and_encode(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    patch: Option<&ErasedApiOptionsPatch>,
    endpoint: &Url,
    auth_headers: &HeaderMap,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::BedrockConverse(config) = &model.api else {
        return Err(invalid_request(
            model,
            format!(
                "model uses API {}, not bedrock-converse-stream",
                model.api.api_id()
            ),
        ));
    };
    let typed = TypedModelDescriptor::<BedrockConverseStream> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compatibility = BedrockConverseStream::resolve_compat(endpoint, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let (estimated_input_tokens, available_context_tokens) =
        if model.common.limits.context_window == 0 {
            (0, 0)
        } else {
            let estimate = estimate_context_tokens(context)
                .map_err(|error| invalid_request(model, error.to_string()))?;
            let available = model
                .common
                .limits
                .context_window
                .saturating_sub(estimate.tokens)
                .saturating_sub(CONTEXT_SAFETY_TOKENS);
            (estimate.tokens, available)
        };
    let patch = parse_patch(model, patch)?;
    let mut options = BedrockConverseStream::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &compatibility,
            effective_base_url: endpoint,
            estimated_input_tokens,
            available_context_tokens,
        },
        simple,
        &patch,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    apply_provider_environment(model, &mut options, auth_headers)?;
    encode_options(model, context, endpoint, typed, compatibility, &options)
}

fn encode_full(
    model: &ModelDescriptor,
    context: &Context,
    options: &ErasedApiFullOptions,
    endpoint: &Url,
    auth_headers: &HeaderMap,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::BedrockConverse(config) = &model.api else {
        return Err(invalid_request(
            model,
            format!(
                "model uses API {}, not bedrock-converse-stream",
                model.api.api_id()
            ),
        ));
    };
    let mut options = options
        .downcast_ref::<BedrockConverseStream>()
        .ok_or_else(|| invalid_request(model, "invalid bedrock-converse-stream options type"))?
        .clone();
    apply_provider_environment(model, &mut options, auth_headers)?;
    let typed = TypedModelDescriptor::<BedrockConverseStream> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compatibility = BedrockConverseStream::resolve_compat(endpoint, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    encode_options(model, context, endpoint, typed, compatibility, &options)
}

fn apply_provider_environment(
    model: &ModelDescriptor,
    options: &mut BedrockOptions,
    auth_headers: &HeaderMap,
) -> Result<(), AiError> {
    let config = signing_config_from_headers(auth_headers)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    if options
        .region
        .as_deref()
        .is_none_or(|region| region.is_empty())
    {
        options.region.clone_from(&config.region);
    }
    options.provider_environment.long_cache_retention = config.long_cache_retention;
    options.provider_environment.force_prompt_caching = config.force_prompt_caching;
    Ok(())
}

fn encode_options(
    model: &ModelDescriptor,
    context: &Context,
    endpoint: &Url,
    typed: TypedModelDescriptor<BedrockConverseStream>,
    compatibility: agentprism_ai::BedrockCompat,
    options: &BedrockOptions,
) -> Result<ProviderPayload, AiError> {
    let projected =
        transform_context_for_model(context, model, &Default::default(), &BedrockHandoff)
            .map_err(|error| invalid_request(model, error.to_string()))?
            .context;
    let wire = BedrockConverseStream::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compatibility,
            effective_base_url: endpoint,
        },
        options,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    Ok(ProviderPayload::typed::<BedrockConverseStream, _>(
        Method::POST,
        typed,
        wire,
        |request| {
            OrderedJsonWriter::to_vec(&request.clone().into()).map_err(|error| {
                MiddlewareError::new(
                    "provider_payload_encode",
                    format!("failed to encode Bedrock command input: {error}"),
                )
            })
        },
    ))
}

fn parse_patch(
    model: &ModelDescriptor,
    patch: Option<&ErasedApiOptionsPatch>,
) -> Result<BedrockSimplePatch, AiError> {
    let Some(patch) = patch else {
        return Ok(BedrockSimplePatch::default());
    };
    if patch.schema_version != 1 {
        return Err(invalid_request(
            model,
            format!(
                "unsupported bedrock-converse-stream options schema version {}",
                patch.schema_version
            ),
        ));
    }
    serde_json::from_str(patch.value.get())
        .map_err(|error| invalid_request(model, format!("invalid API options patch: {error}")))
}

fn invalid_request(model: &ModelDescriptor, message: impl Into<String>) -> AiError {
    AiError::new(AiErrorKind::InvalidRequest, message).with_model(model.common.model_ref.clone())
}

fn decode_context(model: &ModelDescriptor) -> BedrockDecodeContext {
    BedrockDecodeContext {
        message_id: MessageId::new(format!(
            "bedrock-message-{}",
            NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
        )),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        pricing: model.common.pricing.clone(),
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

struct SendDecodeState {
    body: HttpBody,
    decoder: BedrockConverseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<agentprism_ai::AssistantEvent>,
    done: bool,
}

struct LocalDecodeState {
    body: LocalHttpBody,
    decoder: BedrockConverseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<agentprism_ai::AssistantEvent>,
    done: bool,
}

enum BodyPoll {
    Cancelled,
    Body(Option<Result<Vec<u8>, agentprism_ai::TransportError>>),
}

fn decode_send_body(
    body: HttpBody,
    mut decoder: BedrockConverseDecoder,
    cancellation: CancellationToken,
) -> AssistantStream {
    let pending = decoder.take_events().into();
    AssistantStream::new(stream::unfold(
        SendDecodeState {
            body,
            decoder,
            cancellation,
            pending,
            done: false,
        },
        next_send_event,
    ))
}

fn decode_local_body(
    body: LocalHttpBody,
    mut decoder: BedrockConverseDecoder,
    cancellation: CancellationToken,
) -> LocalAssistantStream {
    let pending = decoder.take_events().into();
    LocalAssistantStream::new(stream::unfold(
        LocalDecodeState {
            body,
            decoder,
            cancellation,
            pending,
            done: false,
        },
        next_local_event,
    ))
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
                for diagnostic in std::mem::take(&mut error.diagnostics) {
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
                for diagnostic in std::mem::take(&mut error.diagnostics) {
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

const BEDROCK_DATA_RETENTION_DOCS_URL: &str =
    "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";
const MAX_PROVIDER_ERROR_BODY_UTF16_UNITS: usize = 4_000;

/// Provider rejection exposed by an AWS Bedrock SDK/signer implementation.
///
/// `service_exception` is `Some` only when the source was an AWS
/// `BedrockRuntimeServiceException`; that distinction controls Pi's stable
/// human-readable exception prefix. `body` is the raw or JSON-stringified
/// provider body retained by the SDK.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BedrockProviderFailure {
    /// Modeled AWS service-exception name, when applicable.
    pub service_exception: Option<String>,
    /// Provider error code/name, including unmodeled event-stream exceptions.
    pub provider_code: Option<String>,
    /// SDK error message before Bedrock-specific normalization.
    pub message: String,
    /// HTTP status hidden by the SDK exception boundary.
    pub status: Option<u16>,
    /// Raw or JSON-stringified provider response body.
    pub body: Option<String>,
    /// AWS request identifier, when reported.
    pub request_id: Option<String>,
}

impl BedrockProviderFailure {
    /// Creates an unmodeled provider failure without a service prefix.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            service_exception: None,
            provider_code: None,
            message: message.into(),
            status: None,
            body: None,
            request_id: None,
        }
    }

    /// Creates a modeled AWS Bedrock service exception.
    pub fn service(exception: impl Into<String>, message: impl Into<String>) -> Self {
        let exception = exception.into();
        Self {
            service_exception: Some(exception.clone()),
            provider_code: Some(exception),
            ..Self::new(message)
        }
    }

    /// Retains an unmodeled provider error code without applying a service prefix.
    pub fn with_provider_code(mut self, provider_code: impl Into<String>) -> Self {
        self.provider_code = Some(provider_code.into());
        self
    }

    /// Retains the HTTP status hidden by the SDK.
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// Retains the raw or JSON-stringified provider body.
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Retains the AWS request identifier.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }
}

/// Failure returned by a Bedrock SDK/signer implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BedrockSignerError {
    /// Network, timeout, or host-transport failure with no provider response.
    Transport(TransportError),
    /// Provider response converted into an SDK exception.
    Provider(BedrockProviderFailure),
}

impl From<TransportError> for BedrockSignerError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl From<BedrockProviderFailure> for BedrockSignerError {
    fn from(error: BedrockProviderFailure) -> Self {
        Self::Provider(error)
    }
}

impl fmt::Display for BedrockSignerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&normalize_bedrock_signer_error(self.clone()).message)
    }
}

impl std::error::Error for BedrockSignerError {}

/// Send-capable body returned by a Bedrock signer/client.
pub type BedrockSignerBody = SendBoxStream<'static, Result<Vec<u8>, BedrockSignerError>>;

/// Local-executor body returned by a Bedrock signer/client.
pub type LocalBedrockSignerBody = LocalBoxStream<'static, Result<Vec<u8>, BedrockSignerError>>;

/// Raw Send-capable Bedrock response before provider-error normalization.
pub struct BedrockSignerResponse {
    /// Raw HTTP status.
    pub status: u16,
    /// Raw HTTP response headers.
    pub headers: HeaderMap,
    /// Redacted transport recovery diagnostics.
    pub diagnostics: Vec<AssistantMessageDiagnostic>,
    /// Whether response observers see this exchange.
    pub notify_observers: bool,
    /// Unconsumed AWS event-stream body.
    pub body: BedrockSignerBody,
}

impl BedrockSignerResponse {
    /// Creates an empty successful response.
    pub fn empty(status: u16, headers: HeaderMap) -> Self {
        Self {
            status,
            headers,
            diagnostics: Vec::new(),
            notify_observers: true,
            body: Box::pin(stream::empty()),
        }
    }

    fn into_http_response(self, secret_values: Vec<String>) -> HttpResponse {
        HttpResponse {
            status: self.status,
            headers: self.headers,
            diagnostics: self.diagnostics,
            notify_observers: self.notify_observers,
            decode_non_success: false,
            body: Box::pin(self.body.map(move |item| {
                item.map_err(|error| {
                    normalize_bedrock_signer_error(error).sanitized(&secret_values)
                })
            })),
        }
    }
}

impl From<HttpResponse> for BedrockSignerResponse {
    fn from(response: HttpResponse) -> Self {
        Self {
            status: response.status,
            headers: response.headers,
            diagnostics: response.diagnostics,
            notify_observers: response.notify_observers,
            body: Box::pin(response.body.map(|item| item.map_err(Into::into))),
        }
    }
}

/// Raw local-executor Bedrock response before provider-error normalization.
pub struct LocalBedrockSignerResponse {
    /// Raw HTTP status.
    pub status: u16,
    /// Raw HTTP response headers.
    pub headers: HeaderMap,
    /// Redacted transport recovery diagnostics.
    pub diagnostics: Vec<AssistantMessageDiagnostic>,
    /// Whether response observers see this exchange.
    pub notify_observers: bool,
    /// Unconsumed AWS event-stream body.
    pub body: LocalBedrockSignerBody,
}

impl LocalBedrockSignerResponse {
    /// Creates an empty successful local response.
    pub fn empty(status: u16, headers: HeaderMap) -> Self {
        Self {
            status,
            headers,
            diagnostics: Vec::new(),
            notify_observers: true,
            body: Box::pin(stream::empty()),
        }
    }

    fn into_http_response(self, secret_values: Vec<String>) -> LocalHttpResponse {
        LocalHttpResponse {
            status: self.status,
            headers: self.headers,
            diagnostics: self.diagnostics,
            notify_observers: self.notify_observers,
            decode_non_success: false,
            body: Box::pin(self.body.map(move |item| {
                item.map_err(|error| {
                    normalize_bedrock_signer_error(error).sanitized(&secret_values)
                })
            })),
        }
    }
}

impl From<LocalHttpResponse> for LocalBedrockSignerResponse {
    fn from(response: LocalHttpResponse) -> Self {
        Self {
            status: response.status,
            headers: response.headers,
            diagnostics: response.diagnostics,
            notify_observers: response.notify_observers,
            body: Box::pin(response.body.map(|item| item.map_err(Into::into))),
        }
    }
}

fn normalize_bedrock_signer_error(error: BedrockSignerError) -> TransportError {
    let failure = match error {
        BedrockSignerError::Transport(error) => return error,
        BedrockSignerError::Provider(failure) => failure,
    };

    let body = failure
        .body
        .as_deref()
        .and_then(normalize_provider_error_body);
    let core = match (&body, failure.status) {
        (Some(body), Some(status)) if !failure.message.contains(body) => {
            format!("{status}: {body}")
        }
        _ => failure.message.clone(),
    };
    let hint = if core.to_ascii_lowercase().contains("data retention mode") {
        format!(" See {BEDROCK_DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        String::new()
    };
    let message = match failure.service_exception.as_deref() {
        Some(exception) => format!("{}: {core}{hint}", bedrock_error_prefix(exception)),
        None => format!("{core}{hint}"),
    };
    let mut normalized = TransportError::new("bedrock_provider", message);
    if let Some(provider_code) = failure
        .provider_code
        .as_deref()
        .and_then(normalize_bedrock_provider_code)
    {
        normalized = normalized.with_provider_code(provider_code);
    }
    if let Some(status) = failure.status {
        normalized = normalized.with_status(status);
    }
    if let Some(request_id) = failure
        .request_id
        .as_deref()
        .and_then(normalize_diagnostic_value)
    {
        normalized = normalized.with_request_id(request_id);
    }
    normalized
}

fn bedrock_error_prefix(exception: &str) -> &str {
    match exception {
        "InternalServerException" => "Internal server error",
        "ModelStreamErrorException" => "Model stream error",
        "ValidationException" => "Validation error",
        "ThrottlingException" => "Throttling error",
        "ServiceUnavailableException" => "Service unavailable",
        other => other,
    }
}

fn normalize_bedrock_provider_code(code: &str) -> Option<String> {
    code.ends_with("Exception")
        .then_some(code)
        .and_then(normalize_diagnostic_value)
}

fn normalize_diagnostic_value(value: &str) -> Option<String> {
    let trimmed = trim_ecmascript(value);
    (!trimmed.is_empty() && trimmed.encode_utf16().count() <= 200).then(|| trimmed.to_owned())
}

fn normalize_provider_error_body(body: &str) -> Option<String> {
    let trimmed = trim_ecmascript(body);
    if trimmed.is_empty() {
        return None;
    }
    let units = trimmed.encode_utf16().count();
    if units <= MAX_PROVIDER_ERROR_BODY_UTF16_UNITS {
        return Some(trimmed.to_owned());
    }
    let mut retained = String::new();
    let mut retained_units = 0;
    for character in trimmed.chars() {
        let width = character.len_utf16();
        if retained_units + width > MAX_PROVIDER_ERROR_BODY_UTF16_UNITS {
            break;
        }
        retained.push(character);
        retained_units += width;
    }
    Some(format!(
        "{retained}... [truncated {} chars]",
        units - retained_units
    ))
}

/// Send-capable AWS Bedrock signer/client boundary.
///
/// Implementations receive the usable profile, region, endpoint, static
/// credentials or bearer token selected by the provider. They must apply
/// SigV4 (or bearer auth) only after the request already contains Pi's allowed
/// logical headers. When `config.endpoint` is `None`, implementations must let
/// the AWS region/profile chain resolve the standard service origin rather
/// than treating the catalog origin on `request.url` as an endpoint override.
pub trait BedrockSigner: Send + Sync + 'static {
    /// Signs and executes one serialized Converse Stream request.
    fn execute(
        &self,
        config: crate::BedrockSigningConfig,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<BedrockSignerResponse, BedrockSignerError>>;
}

/// Local-executor AWS Bedrock signer/client boundary.
pub trait LocalBedrockSigner: 'static {
    /// Signs and executes one serialized Converse Stream request.
    fn execute(
        &self,
        config: crate::BedrockSigningConfig,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalBedrockSignerResponse, BedrockSignerError>>;
}

/// Smithy build-stage decorator: serializes the logical command input, inserts
/// caller headers, then delegates to the injected SigV4-capable signer.
#[derive(Clone)]
pub struct BedrockSignerTransport {
    inner: Arc<dyn BedrockSigner>,
}

impl BedrockSignerTransport {
    /// Wraps an AWS signer-capable Send transport.
    pub fn new(inner: Arc<dyn BedrockSigner>) -> Self {
        Self { inner }
    }
}

impl fmt::Debug for BedrockSignerTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BedrockSignerTransport")
            .finish_non_exhaustive()
    }
}

impl HttpTransport for BedrockSignerTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, agentprism_ai::TransportError>> {
        let mut secret_values = bedrock_request_secret_values(&request);
        let config = match build_bedrock_http_request(&mut request) {
            Ok(config) => config,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        extend_bedrock_signing_secret_values(&mut secret_values, &config);
        let execution = self.inner.execute(config, request, cancellation);
        Box::pin(async move {
            match execution.await {
                Ok(response) => Ok(response.into_http_response(secret_values)),
                Err(error) => Err(normalize_bedrock_signer_error(error).sanitized(&secret_values)),
            }
        })
    }
}

/// Local-executor Smithy build-stage decorator.
#[derive(Clone)]
pub struct LocalBedrockSignerTransport {
    inner: Rc<dyn LocalBedrockSigner>,
}

impl LocalBedrockSignerTransport {
    /// Wraps an AWS signer-capable local transport.
    pub fn new(inner: Rc<dyn LocalBedrockSigner>) -> Self {
        Self { inner }
    }
}

impl fmt::Debug for LocalBedrockSignerTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalBedrockSignerTransport")
            .finish_non_exhaustive()
    }
}

impl LocalHttpTransport for LocalBedrockSignerTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, agentprism_ai::TransportError>> {
        let mut secret_values = bedrock_request_secret_values(&request);
        let config = match build_bedrock_http_request(&mut request) {
            Ok(config) => config,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        extend_bedrock_signing_secret_values(&mut secret_values, &config);
        let execution = self.inner.execute(config, request, cancellation);
        Box::pin(async move {
            match execution.await {
                Ok(response) => Ok(response.into_http_response(secret_values)),
                Err(error) => Err(normalize_bedrock_signer_error(error).sanitized(&secret_values)),
            }
        })
    }
}

fn bedrock_request_secret_values(request: &HttpRequest) -> Vec<String> {
    let mut values = request
        .auth_headers
        .values()
        .filter_map(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    values.extend(
        request
            .headers
            .iter()
            .filter(|(name, _)| is_sensitive_header_name(name.as_str()))
            .filter_map(|(_, value)| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    );
    values
}

fn extend_bedrock_signing_secret_values(
    values: &mut Vec<String>,
    config: &crate::BedrockSigningConfig,
) {
    if let Some(credentials) = &config.credentials {
        values.push(credentials.access_key_id.expose_secret().to_owned());
        values.push(credentials.secret_access_key.expose_secret().to_owned());
        if let Some(session_token) = &credentials.session_token {
            values.push(session_token.expose_secret().to_owned());
        }
    }
    if let Some(bearer_token) = &config.bearer_token {
        values.push(bearer_token.expose_secret().to_owned());
    }
    if let Some(proxy_url) = &config.proxy_url {
        if !proxy_url.username().is_empty() {
            values.push(proxy_url.username().to_owned());
        }
        if let Some(password) = proxy_url.password() {
            values.push(password.to_owned());
        }
    }
    values.retain(|value| !value.is_empty());
    values.sort_unstable();
    values.dedup();
}

fn is_sensitive_header_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cf-aig-authorization"
            | "x-api-key"
            | "api-key"
            | "cookie"
            | "set-cookie"
    )
}

fn build_bedrock_http_request(
    request: &mut HttpRequest,
) -> Result<crate::BedrockSigningConfig, agentprism_ai::TransportError> {
    let config = signing_config_from_headers(&request.auth_headers)
        .map_err(|error| agentprism_ai::TransportError::new("bedrock_auth", error.to_string()))?;
    let value = parse_ordered_json(&request.body).map_err(|error| {
        agentprism_ai::TransportError::new(
            "bedrock_serialize",
            format!("invalid command input: {error}"),
        )
    })?;
    let OrderedJsonValue::Object(mut command) = value else {
        return Err(agentprism_ai::TransportError::new(
            "bedrock_serialize",
            "Bedrock command input is not an object",
        ));
    };
    let model_id = command
        .remove("modelId")
        .and_then(|value| value.as_string().cloned())
        .and_then(|value| value.to_utf8().ok())
        .ok_or_else(|| {
            agentprism_ai::TransportError::new("bedrock_serialize", "Bedrock command omits modelId")
        })?;
    request.body = OrderedJsonWriter::to_vec(&command.into()).map_err(|error| {
        agentprism_ai::TransportError::new(
            "bedrock_serialize",
            format!("failed to serialize Bedrock command: {error}"),
        )
    })?;
    let mut segments = request.url.path_segments_mut().map_err(|()| {
        agentprism_ai::TransportError::new(
            "bedrock_endpoint",
            "Bedrock endpoint cannot be a base URL",
        )
    })?;
    segments
        .pop_if_empty()
        .push("model")
        .push(&model_id)
        .push("converse-stream");
    drop(segments);

    // The private carrier lives only in `auth_headers`; every value present in
    // the mutable logical map has later-overlay provenance, even when a
    // transform inserted a byte-for-byte `HeaderValue::clone` of the carrier.
    let logical = request.headers.clone();
    let mut serialized = HeaderMap::new();
    serialized.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    serialized.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
    apply_bedrock_signer_headers(&logical, &mut serialized);
    for (name, value) in &request.auth_headers {
        if name == header::AUTHORIZATION {
            serialized.insert(name, value.clone());
        }
    }
    request.auth_headers.remove(&SIGNING_CONFIG_HEADER);
    request.headers = serialized;
    Ok(config)
}

/// Creates a shared Bedrock API backed by an AWS signer-capable transport.
pub fn bedrock_converse_stream_api(transport: Arc<dyn BedrockSigner>) -> Arc<dyn ChatApi> {
    Arc::new(HttpChatApi::new(
        Arc::new(BedrockConverseHandler::default()),
        Arc::new(BedrockSignerTransport::new(transport)),
    ))
}

/// Creates a local Bedrock API backed by an AWS signer-capable transport.
pub fn local_bedrock_converse_stream_api(
    transport: Rc<dyn LocalBedrockSigner>,
) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalHttpChatApi::new(
        Rc::new(BedrockConverseHandler::default()),
        Rc::new(LocalBedrockSignerTransport::new(transport)),
    ))
}

/// Builds the built-in Amazon Bedrock provider registration.
pub fn bedrock_provider(
    transport: Arc<dyn BedrockSigner>,
) -> Result<ProviderRegistration, BedrockProviderError> {
    ProviderRegistration::builder("amazon-bedrock")
        .display_name("Amazon Bedrock")
        .auth(bedrock_auth_resolver())
        .models(bedrock_models())
        .api(
            BedrockConverseStream::API_ID,
            bedrock_converse_stream_api(transport),
        )
        .build()
        .map_err(BedrockProviderError::Registration)
}

/// Builds the local-executor Amazon Bedrock provider registration.
pub fn local_bedrock_provider(
    transport: Rc<dyn LocalBedrockSigner>,
) -> Result<LocalProviderRegistration, BedrockProviderError> {
    LocalProviderRegistration::builder("amazon-bedrock")
        .display_name("Amazon Bedrock")
        .auth(local_bedrock_auth_resolver())
        .models(bedrock_models())
        .api(
            BedrockConverseStream::API_ID,
            local_bedrock_converse_stream_api(transport),
        )
        .build()
        .map_err(BedrockProviderError::Registration)
}

/// Failure while building the built-in Bedrock registration.
#[derive(Debug)]
pub enum BedrockProviderError {
    /// Provider registration invariants failed.
    Registration(ProviderRegistrationError),
}

impl fmt::Display for BedrockProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registration(error) => write!(formatter, "registration error: {error}"),
        }
    }
}

impl std::error::Error for BedrockProviderError {}
