//! ChatGPT Codex Responses transport ⇐ pi `src/api/openai-codex-responses.ts`.
//!
//! Both transports decode Codex wire frames into Responses events and pass them directly to
//! `openai_responses_shared`; the donor's intermediate `ChatStreamEvent` layer is intentionally
//! absent. Runtime failures settle the returned assistant stream in-band.

mod transport;

pub use transport::OpenAICodexWebSocketDebugStats;

use crate::api::constrained_sampling::create_grammar_tool_input_properties;
use crate::api::openai_prompt_cache::clamp_open_ai_prompt_cache_key;
use crate::api::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ConvertResponsesToolsOptions, DeferredResponsesToolsMode,
    OpenAIResponsesStreamOptions, ResponseServiceTier, ResponseToolChoiceMode,
    convert_responses_messages, convert_responses_tools, process_responses_stream,
};
use crate::api::simple_options::build_base_options;
use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::event_stream::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantStreamSender,
};
use crate::models::clamp_thinking_level;
use crate::session_resources::register_session_resource_cleanup;
use crate::types::{
    AbortSignal, AssistantMessage, AssistantMessageDiagnostic, CacheRetention, Context,
    DiagnosticCode, DiagnosticErrorInfo, ErrorStopReason, Message, Model, ModelThinkingLevel,
    ProviderHttpRequest, ProviderHttpResponse, ProviderResponse, SimpleStreamOptions, StopReason,
    StreamOptions, SuccessfulStopReason, ThinkingLevel, ToolChoice, Transport, Usage,
    serialize_optional_js_f64,
};
#[cfg(test)]
use crate::types::{AssistantContent, ModelCompat, Tool};
use crate::utils::deferred_tools::split_deferred_tools_identity;
use crate::utils::headers::headers_to_record;
use crate::utils::pi_user_agent::get_pi_user_agent;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use futures::{FutureExt, StreamExt};
use http::{HeaderMap, HeaderValue};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Cursor;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use transport::{
    acquire_websocket, build_sse_headers, build_websocket_headers, cached_request_body,
    classify_failure, clear_continuation, fallback_active, headers_to_record as wire_headers,
    record_sse_fallback, record_websocket_failure, record_websocket_request, release_websocket,
    resolve_codex_url, resolve_codex_websocket_url, response_create_frame, send_websocket_frame,
    set_continuation, sse_event_stream, take_failure, websocket_event_stream, websocket_request_id,
};

const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEFAULT_MAX_RETRIES: u32 = 0;
const BASE_DELAY_MS: u64 = 1_000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const REQUEST_COMPRESSION_ZSTD_LEVEL: i32 = 3;
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const PREVIOUS_RESPONSE_NOT_FOUND_CODE: &str = "previous_response_not_found";
const CODEX_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexReasoningSummary {
    Auto,
    Concise,
    Detailed,
    Off,
    On,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexTextVerbosity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICodexResponsesOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<CodexReasoningEffort>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub reasoning_summary: Option<Option<CodexReasoningSummary>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub service_tier: Option<Option<ResponseServiceTier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_verbosity: Option<CodexTextVerbosity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponseToolChoiceMode>,
}

impl From<StreamOptions> for OpenAICodexResponsesOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAICodexResponsesApi;

pub fn open_ai_codex_responses_api() -> OpenAICodexResponsesApi {
    ensure_cleanup_registered();
    OpenAICodexResponsesApi
}

impl ProviderStreams for OpenAICodexResponsesApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        match options {
            ApiStreamOptions::Base(options) => stream(model, context, options.into()),
            ApiStreamOptions::OpenAICodexResponses(options) => stream(model, context, options),
            ApiStreamOptions::OpenAICompletions(_)
            | ApiStreamOptions::OpenAIResponses(_)
            | ApiStreamOptions::Custom { .. } => terminal_setup_error(
                model,
                "API options variant does not match openai-codex-responses",
            ),
        }
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        stream_simple(model, context, options)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexErrorKind {
    Other,
    Aborted,
    Transport,
    Protocol,
    Api { code: Option<String> },
    RetryDelay,
}

#[derive(Debug, Clone)]
struct CodexRunError {
    message: String,
    kind: CodexErrorKind,
    diagnostic_name: &'static str,
    websocket_close_code: Option<u16>,
    stack: String,
}

impl CodexRunError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: CodexErrorKind::Other,
            diagnostic_name: "Error",
            websocket_close_code: None,
            stack: std::backtrace::Backtrace::force_capture().to_string(),
        }
    }

    fn aborted(message: impl Into<String>) -> Self {
        Self {
            kind: CodexErrorKind::Aborted,
            ..Self::new(message)
        }
    }

    fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: CodexErrorKind::Transport,
            ..Self::new(message)
        }
    }

    fn websocket_close(message: impl Into<String>, code: Option<u16>) -> Self {
        Self {
            kind: CodexErrorKind::Transport,
            diagnostic_name: "WebSocketCloseError",
            websocket_close_code: code,
            ..Self::new(message)
        }
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: CodexErrorKind::Protocol,
            diagnostic_name: "CodexProtocolError",
            ..Self::new(message)
        }
    }

    fn api(message: impl Into<String>, code: Option<String>) -> Self {
        Self {
            kind: CodexErrorKind::Api { code },
            diagnostic_name: "CodexApiError",
            ..Self::new(message)
        }
    }

    fn retry_delay(message: impl Into<String>) -> Self {
        Self {
            kind: CodexErrorKind::RetryDelay,
            diagnostic_name: "RetryDelayExceededError",
            ..Self::new(message)
        }
    }

    fn display(error: impl fmt::Display) -> Self {
        Self::new(error.to_string())
    }

    fn transport_display(error: impl fmt::Display) -> Self {
        Self::transport(error.to_string())
    }

    fn code(&self) -> Option<&str> {
        match &self.kind {
            CodexErrorKind::Api { code } => code.as_deref(),
            CodexErrorKind::Other
            | CodexErrorKind::Aborted
            | CodexErrorKind::Transport
            | CodexErrorKind::Protocol
            | CodexErrorKind::RetryDelay => None,
        }
    }

    fn diagnostic_code(&self) -> Option<DiagnosticCode> {
        self.websocket_close_code
            .map(|code| DiagnosticCode::Number(serde_json::Number::from(code)))
            .or_else(|| {
                self.code()
                    .map(|code| DiagnosticCode::String(code.to_owned()))
            })
    }
}

impl fmt::Display for CodexRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodexRunError {}

pub fn stream(
    model: &Model,
    context: &Context,
    options: OpenAICodexResponsesOptions,
) -> AssistantMessageEventStream {
    ensure_cleanup_registered();
    let model = model.clone();
    let context = context.clone();
    let (sender, stream) = AssistantMessageEventStream::channel();
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return terminal_setup_error(&model, "Tokio runtime is not available");
    };
    runtime.spawn(async move {
        run_stream(sender, model, context, options).await;
    });
    stream
}

pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let clamped = options
        .reasoning
        .map(thinking_to_model_level)
        .map(|level| clamp_thinking_level(model, level));
    let api_key = options.stream.request.api_key.clone();
    let mut lowered = OpenAICodexResponsesOptions {
        stream: build_base_options(model, context, Some(&options), api_key.as_deref()),
        reasoning_effort: clamped.and_then(model_level_to_codex_effort),
        tool_choice: options.tool_choice.map(|choice| match choice {
            ToolChoice::Auto => ResponseToolChoiceMode::Auto,
            ToolChoice::None => ResponseToolChoiceMode::None,
        }),
        ..OpenAICodexResponsesOptions::default()
    };
    lowered.stream.request.api_key = api_key;
    stream(model, context, lowered)
}

fn terminal_setup_error(model: &Model, message: &str) -> AssistantMessageEventStream {
    let mut output = pending_message(model);
    output.stop_reason = StopReason::Error;
    output.error_message = Some(message.to_owned());
    AssistantMessageEventStream::from_events(vec![AssistantMessageEvent::Error {
        reason: ErrorStopReason::Error,
        error: output,
    }])
}

fn pending_message(model: &Model) -> AssistantMessage {
    AssistantMessage::pending(
        "openai-codex-responses",
        model.provider.clone(),
        model.id.clone(),
        now_millis(),
    )
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

fn ensure_cleanup_registered() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        register_session_resource_cleanup(Arc::new(|session_id| {
            close_open_ai_codex_websocket_sessions(session_id);
            Ok(())
        }));
    });
}

pub fn get_open_ai_codex_websocket_debug_stats(
    session_id: &str,
) -> Option<OpenAICodexWebSocketDebugStats> {
    transport::get_debug_stats(session_id)
}

pub fn reset_open_ai_codex_websocket_debug_stats(session_id: Option<&str>) {
    transport::reset_debug_stats(session_id);
}

pub fn close_open_ai_codex_websocket_sessions(session_id: Option<&str>) {
    transport::close_sessions(session_id);
}

async fn run_stream(
    sender: AssistantStreamSender,
    model: Model,
    context: Context,
    options: OpenAICodexResponsesOptions,
) {
    let mut output = pending_message(&model);
    let result = AssertUnwindSafe(run_stream_inner(
        &sender,
        &model,
        &context,
        &options,
        &mut output,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|panic| {
        Err(CodexRunError::new(
            crate::utils::diagnostics::format_panic_payload(panic.as_ref()),
        ))
    });
    if let Err(error) = result {
        let aborted = is_aborted(&options) || matches!(error.kind, CodexErrorKind::Aborted);
        output.stop_reason = if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        output.error_message = Some(error.message);
        let _ = sender.send(AssistantMessageEvent::Error {
            reason: if aborted {
                ErrorStopReason::Aborted
            } else {
                ErrorStopReason::Error
            },
            error: output,
        });
    }
}

async fn run_stream_inner(
    sender: &AssistantStreamSender,
    model: &Model,
    context: &Context,
    options: &OpenAICodexResponsesOptions,
    output: &mut AssistantMessage,
) -> Result<(), CodexRunError> {
    let api_key = options
        .stream
        .request
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            CodexRunError::new(format!("No API key for provider: {}", model.provider))
        })?;
    let account_id = extract_account_id(api_key)?;
    let compat = codex_compat(model);
    let grammar_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat.supports_open_ai_grammar_tools.unwrap_or(false),
    )
    .map_err(CodexRunError::display)?;
    let cache_session_id = (options.stream.cache_retention != Some(CacheRetention::None))
        .then_some(options.stream.session_id.as_deref())
        .flatten();
    let transport_session_id = cache_session_id.filter(|session_id| !session_id.is_empty());
    let codex_session_id = clamp_open_ai_prompt_cache_key(cache_session_id);
    let mut body = build_request_body(
        model,
        context,
        options,
        codex_session_id.as_deref(),
        &grammar_properties,
    )?;
    if let Some(on_payload) = &options.stream.request.on_payload
        && let Some(replacement) = on_payload(body.clone(), model).await
    {
        body = replacement;
    }
    let body_json = serde_json::to_string(&body).map_err(CodexRunError::display)?;
    let websocket_request_id = websocket_request_id(codex_session_id.as_deref());
    let user_agent = get_pi_user_agent();
    let mut sse_headers = build_sse_headers(
        model.headers.as_ref(),
        options.stream.request.headers.as_ref(),
        &account_id,
        api_key,
        &user_agent,
        codex_session_id
            .as_deref()
            .filter(|session_id| !session_id.is_empty()),
    )?;
    let websocket_headers = build_websocket_headers(
        model.headers.as_ref(),
        options.stream.request.headers.as_ref(),
        &account_id,
        api_key,
        &user_agent,
        &websocket_request_id,
    )?;
    let transport = options.stream.transport.unwrap_or(Transport::Auto);
    let start_emitted = Arc::new(AtomicBool::new(false));
    let websocket_disabled = transport != Transport::Sse && fallback_active(transport_session_id);
    if websocket_disabled {
        record_sse_fallback(transport_session_id);
    }

    if transport != Transport::Sse && !websocket_disabled {
        let mut retried_connection_limit = false;
        let mut retried_missing_continuation = false;
        loop {
            let attempt_started = Arc::new(AtomicBool::new(false));
            let result = process_websocket_stream(WebSocketAttempt {
                sender,
                model,
                options,
                output,
                body: &body,
                headers: &websocket_headers,
                cache_session_id: transport_session_id,
                account_id: &account_id,
                grammar_properties: &grammar_properties,
                start_emitted: start_emitted.clone(),
                attempt_started: attempt_started.clone(),
            })
            .await
            .and_then(|()| {
                if is_aborted(options) {
                    Err(CodexRunError::aborted("Request was aborted"))
                } else {
                    successful_reason(output)
                }
            });
            match result {
                Ok(reason) => {
                    sender
                        .send(AssistantMessageEvent::Done {
                            reason,
                            message: output.clone(),
                        })
                        .map_err(CodexRunError::display)?;
                    return Ok(());
                }
                Err(error) => {
                    let aborted = is_aborted(options)
                        || matches!(classify_failure(&error), CodexErrorKind::Aborted);
                    let connection_limit_before_start = !attempt_started.load(Ordering::Acquire)
                        && error.code() == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE);
                    if !aborted
                        && error.code() == Some(PREVIOUS_RESPONSE_NOT_FOUND_CODE)
                        && !retried_missing_continuation
                    {
                        retried_missing_continuation = true;
                        continue;
                    }
                    if !aborted && connection_limit_before_start && !retried_connection_limit {
                        retried_connection_limit = true;
                        continue;
                    }
                    if aborted
                        || (matches!(
                            classify_failure(&error),
                            CodexErrorKind::Api { .. } | CodexErrorKind::Protocol
                        ) && !connection_limit_before_start)
                    {
                        return Err(error);
                    }
                    let websocket_started = attempt_started.load(Ordering::Acquire);
                    append_transport_diagnostic(
                        output,
                        transport,
                        websocket_started,
                        body_json.len(),
                        &error,
                    );
                    record_websocket_failure(transport_session_id, &error);
                    if websocket_started {
                        return Err(error);
                    }
                    record_sse_fallback(transport_session_id);
                    break;
                }
            }
        }
    }

    let compressed = zstd::stream::encode_all(
        Cursor::new(body_json.as_bytes()),
        REQUEST_COMPRESSION_ZSTD_LEVEL,
    )
    .ok();
    let compressed_available = compressed.is_some();
    let sse_body = compressed.unwrap_or_else(|| body_json.into_bytes());
    if compressed_available {
        sse_headers.insert("content-encoding", HeaderValue::from_static("zstd"));
    }
    let response = acquire_sse_response(model, options, sse_headers, sse_body).await?;
    let response_body = response
        .body
        .ok_or_else(|| CodexRunError::new("No response body"))?;
    if !start_emitted.swap(true, Ordering::AcqRel) {
        sender
            .send(AssistantMessageEvent::Start)
            .map_err(CodexRunError::display)?;
    }
    let mut events = sse_event_stream(response_body, options.stream.request.signal.clone());
    process_with_shared(
        &mut events,
        output,
        sender,
        model,
        options,
        &grammar_properties,
    )
    .await?;
    if is_aborted(options) {
        return Err(CodexRunError::aborted("Request was aborted"));
    }
    let reason = successful_reason(output)?;
    sender
        .send(AssistantMessageEvent::Done {
            reason,
            message: output.clone(),
        })
        .map_err(CodexRunError::display)
}

fn append_transport_diagnostic(
    output: &mut AssistantMessage,
    transport: Transport,
    started: bool,
    request_bytes: usize,
    error: &CodexRunError,
) {
    let mut details = Map::new();
    details.insert(
        "configuredTransport".to_owned(),
        serde_json::to_value(transport).expect("transport serializes"),
    );
    if !started {
        details.insert(
            "fallbackTransport".to_owned(),
            Value::String("sse".to_owned()),
        );
    }
    details.insert("eventsEmitted".to_owned(), Value::Bool(started));
    details.insert(
        "phase".to_owned(),
        Value::String(if started {
            "after_message_stream_start".to_owned()
        } else {
            "before_message_stream_start".to_owned()
        }),
    );
    details.insert(
        "requestBytes".to_owned(),
        Value::from(u64::try_from(request_bytes).unwrap_or(u64::MAX)),
    );
    let diagnostic = AssistantMessageDiagnostic {
        kind: "provider_transport_failure".to_owned(),
        timestamp: now_millis(),
        error: Some(DiagnosticErrorInfo {
            name: Some(error.diagnostic_name.to_owned()),
            message: error.message.clone(),
            stack: Some(error.stack.clone()),
            code: error.diagnostic_code(),
        }),
        details: Some(details),
    };
    output
        .diagnostics
        .get_or_insert_with(Vec::new)
        .push(diagnostic);
}

fn successful_reason(output: &AssistantMessage) -> Result<SuccessfulStopReason, CodexRunError> {
    match output.stop_reason {
        StopReason::Stop => Ok(SuccessfulStopReason::Stop),
        StopReason::Length => Ok(SuccessfulStopReason::Length),
        StopReason::ToolUse => Ok(SuccessfulStopReason::ToolUse),
        StopReason::Pending => Err(CodexRunError::new(
            "Codex stream ended without a stop reason",
        )),
        StopReason::Error | StopReason::Aborted => Err(CodexRunError::new(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_owned()),
        )),
        StopReason::Deferred => Err(CodexRunError::new(
            "Provider returned an invalid successful stop reason",
        )),
    }
}

struct WebSocketAttempt<'a> {
    sender: &'a AssistantStreamSender,
    model: &'a Model,
    options: &'a OpenAICodexResponsesOptions,
    output: &'a mut AssistantMessage,
    body: &'a Value,
    headers: &'a HeaderMap,
    cache_session_id: Option<&'a str>,
    account_id: &'a str,
    grammar_properties: &'a BTreeMap<String, String>,
    start_emitted: Arc<AtomicBool>,
    attempt_started: Arc<AtomicBool>,
}

async fn process_websocket_stream(attempt: WebSocketAttempt<'_>) -> Result<(), CodexRunError> {
    let WebSocketAttempt {
        sender,
        model,
        options,
        output,
        body,
        headers,
        cache_session_id,
        account_id,
        grammar_properties,
        start_emitted,
        attempt_started,
    } = attempt;
    let url = resolve_codex_websocket_url(&model.base_url)?;
    let websocket_connect_timeout_ms =
        normalize_timeout_ms(options.stream.websocket_connect_timeout_ms)?;
    let idle_timeout_ms = normalize_timeout_ms(options.stream.request.timeout_ms)?;
    let acquired = acquire_websocket(
        &url,
        headers,
        cache_session_id,
        account_id,
        options.stream.request.signal.clone(),
        websocket_connect_timeout_ms,
    )
    .await?;
    let cached_context = matches!(
        options.stream.transport.unwrap_or(Transport::Auto),
        Transport::WebsocketCached | Transport::Auto
    );
    let request_body = if cached_context && acquired.has_cached_entry() {
        cached_request_body(&acquired, body)
    } else {
        body.clone()
    };
    record_websocket_request(cache_session_id, &acquired, cached_context, &request_body);
    let frame = response_create_frame(&request_body)?;
    if let Err(error) =
        send_websocket_frame(&acquired, frame, options.stream.request.signal.clone()).await
    {
        clear_continuation(&acquired);
        release_websocket(&acquired, false);
        return Err(error);
    }
    let failure = Arc::new(Mutex::new(None));
    let mut events = websocket_event_stream(
        &acquired,
        options.stream.request.signal.clone(),
        idle_timeout_ms,
        sender.clone(),
        start_emitted,
        attempt_started,
        failure.clone(),
    );
    let result = process_with_shared(
        &mut events,
        output,
        sender,
        model,
        options,
        grammar_properties,
    )
    .await
    .map_err(|error| take_failure(&failure).unwrap_or(error));
    match result {
        Ok(()) => {
            let keep = !is_aborted(options);
            if keep
                && cached_context
                && acquired.has_cached_entry()
                && let Some(response_id) = output
                    .response_id
                    .clone()
                    .filter(|response_id| !response_id.is_empty())
            {
                let response_items =
                    match response_items_for_continuation(model, output, grammar_properties) {
                        Ok(items) => items,
                        Err(error) => {
                            clear_continuation(&acquired);
                            release_websocket(&acquired, false);
                            return Err(error);
                        }
                    };
                set_continuation(&acquired, body.clone(), response_id, response_items);
            }
            release_websocket(&acquired, keep);
            Ok(())
        }
        Err(error) => {
            clear_continuation(&acquired);
            release_websocket(&acquired, false);
            Err(error)
        }
    }
}

async fn process_with_shared<S, E>(
    events: &mut S,
    output: &mut AssistantMessage,
    sender: &AssistantStreamSender,
    model: &Model,
    options: &OpenAICodexResponsesOptions,
    grammar_properties: &BTreeMap<String, String>,
) -> Result<(), CodexRunError>
where
    S: futures::Stream<Item = Result<Value, E>> + Unpin,
    E: fmt::Display,
{
    let resolve = |response, request| resolve_codex_service_tier(response, request);
    let apply = |usage: &mut Usage, tier| apply_service_tier_pricing(usage, tier, model);
    process_responses_stream(
        events,
        output,
        sender,
        model,
        OpenAIResponsesStreamOptions {
            service_tier: options.service_tier.clone(),
            grammar_tool_input_properties: Some(grammar_properties),
            resolve_service_tier: Some(&resolve),
            apply_service_tier_pricing: Some(&apply),
            capture_end_turn: true,
        },
    )
    .await
    .map_err(CodexRunError::display)
}

fn response_items_for_continuation(
    model: &Model,
    output: &AssistantMessage,
    grammar_properties: &BTreeMap<String, String>,
) -> Result<Vec<Value>, CodexRunError> {
    let allowed = CODEX_TOOL_CALL_PROVIDERS
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let context = Context {
        messages: vec![Message::Assistant(Box::new(output.clone()))],
        ..Context::default()
    };
    convert_responses_messages(
        model,
        &context,
        &allowed,
        ConvertResponsesMessagesOptions {
            include_system_prompt: Some(false),
            grammar_tool_input_properties: Some(grammar_properties),
            ..ConvertResponsesMessagesOptions::default()
        },
    )
    .map_err(CodexRunError::display)?
    .into_iter()
    .map(|item| serde_json::to_value(item).map_err(CodexRunError::display))
    .filter_map(|item| match item {
        Ok(value)
            if matches!(
                value.get("type").and_then(Value::as_str),
                Some("function_call_output" | "custom_tool_call_output")
            ) =>
        {
            None
        }
        item => Some(item),
    })
    .collect()
}

fn codex_compat(model: &Model) -> crate::types::OpenAIResponsesCompat {
    model
        .compat
        .as_ref()
        .and_then(|compat| serde_json::to_value(compat).ok())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

#[derive(Serialize)]
struct CodexRequestBody {
    model: String,
    store: bool,
    stream: bool,
    instructions: String,
    input: Vec<crate::api::openai_responses_shared::ResponseInputItem>,
    text: CodexTextOptions,
    include: [&'static str; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    tool_choice: ResponseToolChoiceMode,
    parallel_tool_calls: bool,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_js_f64"
    )]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<Option<ResponseServiceTier>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<crate::api::openai_responses_shared::ResponseTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<CodexReasoningOptions>,
}

#[derive(Serialize)]
struct CodexTextOptions {
    verbosity: CodexTextVerbosity,
}

#[derive(Serialize)]
struct CodexReasoningOptions {
    effort: String,
    summary: CodexReasoningSummary,
}

fn build_request_body(
    model: &Model,
    context: &Context,
    options: &OpenAICodexResponsesOptions,
    cache_session_id: Option<&str>,
    grammar_properties: &BTreeMap<String, String>,
) -> Result<Value, CodexRunError> {
    let compat = codex_compat(model);
    let supports_strict = compat.supports_strict_mode.unwrap_or(true);
    let supports_grammar = compat.supports_open_ai_grammar_tools.unwrap_or(false);
    let deferred_mode = if compat.supports_additional_tools.unwrap_or(false) {
        Some(DeferredResponsesToolsMode::AdditionalTools)
    } else if compat.supports_tool_search.unwrap_or(false) {
        Some(DeferredResponsesToolsMode::ToolSearch)
    } else {
        None
    };
    let split = split_deferred_tools_identity(context, deferred_mode.is_some());
    let immediate = split.immediate;
    let deferred = split.deferred.into_iter().collect::<Vec<_>>();
    let allowed = CODEX_TOOL_CALL_PROVIDERS
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let tool_options = ConvertResponsesToolsOptions {
        strict: Some(None),
        supports_strict_mode: Some(supports_strict),
        supports_open_ai_grammar_tools: Some(supports_grammar),
        defer_loading: false,
    };
    let input = convert_responses_messages(
        model,
        context,
        &allowed,
        ConvertResponsesMessagesOptions {
            include_system_prompt: Some(false),
            grammar_tool_input_properties: Some(grammar_properties),
            deferred_tools: Some(&deferred),
            deferred_tools_mode: deferred_mode,
            tool_options: tool_options.clone(),
        },
    )
    .map_err(CodexRunError::display)?;
    let tools = if immediate.is_empty() {
        None
    } else {
        Some(
            convert_responses_tools(immediate.iter(), &tool_options)
                .map_err(CodexRunError::display)?,
        )
    };
    let reasoning = if let Some(requested) = options.reasoning_effort {
        let effort = resolve_reasoning_effort(model, requested);
        let summary = options
            .reasoning_summary
            .flatten()
            .unwrap_or(CodexReasoningSummary::Auto);
        Some(CodexReasoningOptions { effort, summary })
    } else {
        None
    };
    serde_json::to_value(CodexRequestBody {
        model: model.id.clone(),
        store: false,
        stream: true,
        instructions: context
            .system_prompt
            .as_deref()
            .filter(|prompt| !prompt.is_empty())
            .unwrap_or("You are a helpful assistant.")
            .to_owned(),
        input,
        text: CodexTextOptions {
            verbosity: options.text_verbosity.unwrap_or(CodexTextVerbosity::Low),
        },
        include: ["reasoning.encrypted_content"],
        prompt_cache_key: cache_session_id.map(str::to_owned),
        tool_choice: options.tool_choice.unwrap_or(ResponseToolChoiceMode::Auto),
        parallel_tool_calls: true,
        temperature: options.stream.temperature,
        service_tier: options.service_tier.clone(),
        tools,
        reasoning,
    })
    .map_err(CodexRunError::display)
}

fn resolve_reasoning_effort(model: &Model, requested: CodexReasoningEffort) -> String {
    let mapped: Option<Option<String>> = match requested {
        CodexReasoningEffort::None => model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.off.clone()),
        CodexReasoningEffort::Minimal => thinking_mapping(model, ThinkingLevel::Minimal),
        CodexReasoningEffort::Low => thinking_mapping(model, ThinkingLevel::Low),
        CodexReasoningEffort::Medium => thinking_mapping(model, ThinkingLevel::Medium),
        CodexReasoningEffort::High => thinking_mapping(model, ThinkingLevel::High),
        CodexReasoningEffort::Xhigh => thinking_mapping(model, ThinkingLevel::Xhigh),
        CodexReasoningEffort::Max => thinking_mapping(model, ThinkingLevel::Max),
    };
    mapped
        .flatten()
        .unwrap_or_else(|| reasoning_effort_name(requested).to_owned())
}

fn thinking_mapping(model: &Model, level: ThinkingLevel) -> Option<Option<String>> {
    let map = model.thinking_level_map.as_ref()?;
    match level {
        ThinkingLevel::Minimal => map.minimal.clone(),
        ThinkingLevel::Low => map.low.clone(),
        ThinkingLevel::Medium => map.medium.clone(),
        ThinkingLevel::High => map.high.clone(),
        ThinkingLevel::Xhigh => map.xhigh.clone(),
        ThinkingLevel::Max => map.max.clone(),
    }
}

fn reasoning_effort_name(effort: CodexReasoningEffort) -> &'static str {
    match effort {
        CodexReasoningEffort::None => "none",
        CodexReasoningEffort::Minimal => "minimal",
        CodexReasoningEffort::Low => "low",
        CodexReasoningEffort::Medium => "medium",
        CodexReasoningEffort::High => "high",
        CodexReasoningEffort::Xhigh => "xhigh",
        CodexReasoningEffort::Max => "max",
    }
}

fn thinking_to_model_level(level: ThinkingLevel) -> ModelThinkingLevel {
    match level {
        ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => ModelThinkingLevel::Low,
        ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        ThinkingLevel::High => ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

fn model_level_to_codex_effort(level: ModelThinkingLevel) -> Option<CodexReasoningEffort> {
    match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal => Some(CodexReasoningEffort::Minimal),
        ModelThinkingLevel::Low => Some(CodexReasoningEffort::Low),
        ModelThinkingLevel::Medium => Some(CodexReasoningEffort::Medium),
        ModelThinkingLevel::High => Some(CodexReasoningEffort::High),
        ModelThinkingLevel::Xhigh => Some(CodexReasoningEffort::Xhigh),
        ModelThinkingLevel::Max => Some(CodexReasoningEffort::Max),
    }
}

fn resolve_codex_service_tier(
    response: Option<Option<ResponseServiceTier>>,
    request: Option<Option<ResponseServiceTier>>,
) -> Option<Option<ResponseServiceTier>> {
    if response == Some(Some(ResponseServiceTier::Default))
        && matches!(
            request,
            Some(Some(
                ResponseServiceTier::Flex | ResponseServiceTier::Priority
            ))
        )
    {
        return request;
    }
    match response {
        Some(Some(tier)) => Some(Some(tier)),
        Some(None) | None => request,
    }
}

fn service_tier_multiplier(model: &Model, tier: Option<ResponseServiceTier>) -> f64 {
    match tier {
        Some(ResponseServiceTier::Flex) => 0.5,
        Some(ResponseServiceTier::Priority) if model.id == "gpt-5.5" => 2.5,
        Some(ResponseServiceTier::Priority) => 2.0,
        _ => 1.0,
    }
}

fn apply_service_tier_pricing(
    usage: &mut Usage,
    tier: Option<Option<ResponseServiceTier>>,
    model: &Model,
) {
    let multiplier = service_tier_multiplier(model, tier.flatten());
    if multiplier == 1.0 {
        return;
    }
    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

fn extract_account_id(token: &str) -> Result<String, CodexRunError> {
    let parts = token.split('.').collect::<Vec<_>>();
    let payload = parts
        .get(1)
        .filter(|_| parts.len() == 3)
        .ok_or_else(|| CodexRunError::new("Failed to extract accountId from token"))?;
    let payload = payload
        .chars()
        .filter(|character| !matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{000c}'))
        .collect::<String>();
    let bytes = STANDARD
        .decode(&payload)
        .or_else(|_| STANDARD_NO_PAD.decode(&payload))
        .map_err(|_| CodexRunError::new("Failed to extract accountId from token"))?;
    let json_text = bytes.iter().copied().map(char::from).collect::<String>();
    let value: Value = serde_json::from_str(&json_text)
        .map_err(|_| CodexRunError::new("Failed to extract accountId from token"))?;
    let account_id = value
        .get(JWT_CLAIM_PATH)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .filter(|account_id| js_truthy(account_id))
        .ok_or_else(|| CodexRunError::new("Failed to extract accountId from token"))?;
    Ok(js_string(account_id))
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value
            .as_f64()
            .is_some_and(|value| value != 0.0 && !value.is_nan()),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => crate::utils::error_body::js_number_string(value),
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    js_string(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

fn is_aborted(options: &OpenAICodexResponsesOptions) -> bool {
    options
        .stream
        .request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
}

async fn wait_for_abort(signal: Option<Arc<dyn crate::types::AbortSignal>>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => futures::future::pending::<()>().await,
    }
}

struct HeaderTimeoutSignal {
    upstream: Option<Arc<dyn crate::types::AbortSignal>>,
    deadline: tokio::time::Instant,
    state: AtomicU8,
    changed: tokio::sync::Notify,
}

impl HeaderTimeoutSignal {
    fn mark_aborted(&self) {
        let _ = self
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }

    fn timeout_fired(&self) -> bool {
        if self.state.load(Ordering::Acquire) == 0 && tokio::time::Instant::now() >= self.deadline {
            let _ = self
                .state
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
        }
        self.state.load(Ordering::Acquire) == 1
    }

    fn cleanup(&self) {
        if self
            .state
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.changed.notify_waiters();
        }
    }
}

impl crate::types::AbortSignal for HeaderTimeoutSignal {
    fn is_aborted(&self) -> bool {
        match self.state.load(Ordering::Acquire) {
            1 => true,
            2 => false,
            _ => {
                if self
                    .upstream
                    .as_ref()
                    .is_some_and(|signal| signal.is_aborted())
                {
                    self.mark_aborted();
                    true
                } else {
                    self.timeout_fired()
                }
            }
        }
    }

    fn cancelled(&self) -> futures::future::BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let changed = self.changed.notified();
                match self.state.load(Ordering::Acquire) {
                    1 => return,
                    2 => {
                        futures::future::pending::<()>().await;
                    }
                    _ => {
                        tokio::select! {
                            _ = wait_for_abort(self.upstream.clone()) => {
                                self.mark_aborted();
                                return;
                            }
                            _ = tokio::time::sleep_until(self.deadline) => {
                                if self.timeout_fired() {
                                    return;
                                }
                            }
                            _ = changed => {}
                        }
                    }
                }
            }
        })
    }
}

async fn read_response_body(
    mut body: crate::types::ProviderBodyStream,
    signal: Option<Arc<dyn crate::types::AbortSignal>>,
) -> Result<Vec<u8>, CodexRunError> {
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = wait_for_abort(signal.clone()) => return Err(CodexRunError::aborted("Request was aborted")),
            chunk = body.next() => chunk,
        };
        match chunk {
            Some(Ok(chunk)) => bytes.extend(chunk),
            Some(Err(error)) => return Err(CodexRunError::transport(error)),
            None => return Ok(bytes),
        }
    }
}

async fn send_sse_request(
    options: &OpenAICodexResponsesOptions,
    url: String,
    headers: HeaderMap,
    body: Vec<u8>,
    signal: Option<Arc<dyn crate::types::AbortSignal>>,
) -> Result<ProviderHttpResponse, CodexRunError> {
    if let Some(fetch) = &options.stream.request.fetch {
        return fetch
            .fetch(ProviderHttpRequest {
                method: "POST".to_owned(),
                url,
                headers: wire_headers(&headers),
                body,
                signal,
            })
            .await
            .map_err(CodexRunError::transport);
    }
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let response = CLIENT
        .get_or_init(reqwest::Client::new)
        .post(url)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(CodexRunError::transport_display)?;
    let status = response.status();
    let status_text = reqwest_status_text(&response);
    let response_headers = headers_to_record(response.headers());
    let body = (!matches!(status.as_u16(), 101 | 204 | 205 | 304)).then(|| {
        response
            .bytes_stream()
            .map(|chunk| {
                chunk
                    .map(|bytes| bytes.to_vec())
                    .map_err(|error| error.to_string())
            })
            .boxed()
    });
    Ok(ProviderHttpResponse {
        status: status.as_u16(),
        status_text,
        headers: response_headers,
        body,
    })
}

fn reqwest_status_text(response: &reqwest::Response) -> String {
    if !matches!(
        response.version(),
        http::Version::HTTP_10 | http::Version::HTTP_11
    ) {
        return String::new();
    }
    response
        .extensions()
        .get::<hyper::ext::ReasonPhrase>()
        .map(|reason| String::from_utf8_lossy(reason.as_bytes()).into_owned())
        .unwrap_or_else(|| {
            response
                .status()
                .canonical_reason()
                .unwrap_or_default()
                .to_owned()
        })
}

async fn send_sse_with_header_timeout(
    model: &Model,
    options: &OpenAICodexResponsesOptions,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<ProviderHttpResponse, CodexRunError> {
    let timeout_ms = normalize_timeout_ms(options.stream.request.timeout_ms)?;
    if let Some(timeout_ms) = timeout_ms.filter(|timeout| *timeout > 0) {
        let timeout_signal = Arc::new(HeaderTimeoutSignal {
            upstream: options.stream.request.signal.clone(),
            deadline: tokio::time::Instant::now() + Duration::from_millis(timeout_ms),
            state: AtomicU8::new(0),
            changed: tokio::sync::Notify::new(),
        });
        let request_signal: Arc<dyn crate::types::AbortSignal> = timeout_signal.clone();
        let send = send_sse_request(
            options,
            resolve_codex_url(&model.base_url),
            headers,
            body,
            Some(request_signal),
        );
        let result = tokio::select! {
            _ = wait_for_abort(options.stream.request.signal.clone()) => {
                timeout_signal.mark_aborted();
                Err(CodexRunError::aborted("Request was aborted"))
            },
            result = tokio::time::timeout(Duration::from_millis(timeout_ms), send) => {
                if options.stream.request.signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    timeout_signal.mark_aborted();
                    Err(CodexRunError::aborted("Request was aborted"))
                } else if timeout_signal.is_aborted() {
                    Err(CodexRunError::transport(format!("Codex SSE response headers timed out after {timeout_ms}ms")))
                } else {
                    result.map_err(|_| CodexRunError::transport(format!("Codex SSE response headers timed out after {timeout_ms}ms")))?
                }
            }
        };
        timeout_signal.cleanup();
        result
    } else {
        let send = send_sse_request(
            options,
            resolve_codex_url(&model.base_url),
            headers,
            body,
            options.stream.request.signal.clone(),
        );
        tokio::select! {
            _ = wait_for_abort(options.stream.request.signal.clone()) => Err(CodexRunError::aborted("Request was aborted")),
            result = send => {
                if options.stream.request.signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    Err(CodexRunError::aborted("Request was aborted"))
                } else {
                    result
                }
            },
        }
    }
}

async fn acquire_sse_response(
    model: &Model,
    options: &OpenAICodexResponsesOptions,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<ProviderHttpResponse, CodexRunError> {
    let max_retries = options
        .stream
        .request
        .max_retries
        .unwrap_or(f64::from(DEFAULT_MAX_RETRIES));
    let mut last_error = None;
    let mut attempt = 0_u32;
    while f64::from(attempt) <= max_retries {
        if is_aborted(options) {
            return Err(CodexRunError::aborted("Request was aborted"));
        }
        let response =
            send_sse_with_header_timeout(model, options, headers.clone(), body.clone()).await;
        let attempt_error = match response {
            Ok(response) => {
                if let Some(on_response) = &options.stream.request.on_response {
                    on_response(
                        ProviderResponse {
                            status: response.status,
                            headers: response.headers.clone(),
                        },
                        model,
                    )
                    .await;
                }
                if (200..300).contains(&response.status) {
                    return Ok(response);
                }
                let status = response.status;
                let status_text = response.status_text.clone();
                let response_headers = response.headers.clone();
                let error_body = response
                    .body
                    .unwrap_or_else(|| futures::stream::empty().boxed());
                let error_text =
                    match read_response_body(error_body, options.stream.request.signal.clone())
                        .await
                    {
                        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                        Err(error) => {
                            if matches!(error.kind, CodexErrorKind::Aborted) {
                                return Err(error);
                            }
                            if f64::from(attempt) < max_retries
                                && !matches!(error.kind, CodexErrorKind::RetryDelay)
                                && !error.message.contains("usage limit")
                            {
                                sleep_with_abort(exponential_delay(attempt), options).await?;
                                attempt = attempt.saturating_add(1);
                                continue;
                            }
                            return Err(error);
                        }
                    };
                if f64::from(attempt) < max_retries && is_retryable_error(status, &error_text) {
                    let delay = retry_after_delay_ms(&response_headers)
                        .map(|delay| validate_retry_delay(delay, options));
                    match delay.transpose()? {
                        Some(delay) => sleep_with_abort(delay, options).await?,
                        None => sleep_with_abort(exponential_delay(attempt), options).await?,
                    }
                    attempt = attempt.saturating_add(1);
                    continue;
                }
                CodexRunError::new(parse_error_response(status, &status_text, &error_text))
            }
            Err(error) => error,
        };
        if matches!(attempt_error.kind, CodexErrorKind::Aborted) {
            return Err(CodexRunError::aborted("Request was aborted"));
        }
        last_error = Some(attempt_error.clone());
        if f64::from(attempt) < max_retries
            && !matches!(attempt_error.kind, CodexErrorKind::RetryDelay)
            && !attempt_error.message.contains("usage limit")
        {
            sleep_with_abort(exponential_delay(attempt), options).await?;
            attempt = attempt.saturating_add(1);
            continue;
        }
        return Err(attempt_error);
    }
    Err(last_error.unwrap_or_else(|| CodexRunError::new("Failed after retries")))
}

fn exponential_delay(attempt: u32) -> u64 {
    BASE_DELAY_MS.saturating_mul(2_u64.saturating_pow(attempt))
}

async fn sleep_with_abort(
    delay_ms: u64,
    options: &OpenAICodexResponsesOptions,
) -> Result<(), CodexRunError> {
    let duration = crate::utils::sleep::duration_from_js_timeout(delay_ms as f64);
    tokio::select! {
        _ = wait_for_abort(options.stream.request.signal.clone()) => Err(CodexRunError::aborted("Request was aborted")),
        _ = tokio::time::sleep(duration) => Ok(()),
    }
}

fn is_terminal_rate_limit_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "gousagelimiterror",
        "freeusagelimiterror",
        "monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ]
    .iter()
    .any(|pattern| error.contains(pattern))
}

fn is_retryable_error(status: u16, error: &str) -> bool {
    if status == 429 && is_terminal_rate_limit_error(error) {
        return false;
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    let normalized = error.to_ascii_lowercase();
    normalized.contains("overloaded")
        || matches_optional_character(&normalized, "rate", "limit")
        || matches_optional_character(&normalized, "service", "unavailable")
        || matches_optional_character(&normalized, "upstream", "connect")
        || matches_optional_character(&normalized, "connection", "refused")
}

fn matches_optional_character(value: &str, prefix: &str, suffix: &str) -> bool {
    value.match_indices(prefix).any(|(index, _)| {
        let remainder = &value[index + prefix.len()..];
        remainder.starts_with(suffix)
            || remainder.chars().next().is_some_and(|character| {
                character.len_utf16() == 1
                    && !matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
                    && remainder[character.len_utf8()..].starts_with(suffix)
            })
    })
}

fn retry_after_delay_ms(headers: &BTreeMap<String, String>) -> Option<u64> {
    if let Some(value) = header(headers, "retry-after-ms")
        && let Some(millis) = parse_javascript_number(value)
        && millis.is_finite()
    {
        return duration_millis(millis.max(0.0));
    }
    let value = header(headers, "retry-after")?;
    if let Some(seconds) = parse_javascript_number(value)
        && seconds.is_finite()
    {
        return duration_millis(seconds.max(0.0) * 1_000.0);
    }
    let date = parse_javascript_date_ms(value)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1_000.0;
    duration_millis((date - now).max(0.0))
}

use crate::utils::provider_retry::{parse_javascript_date_ms, parse_javascript_number};

fn duration_millis(millis: f64) -> Option<u64> {
    let duration = Duration::try_from_secs_f64(millis / 1_000.0).ok()?;
    u64::try_from(duration.as_millis()).ok()
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn validate_retry_delay(
    delay_ms: u64,
    options: &OpenAICodexResponsesOptions,
) -> Result<u64, CodexRunError> {
    let maximum = options
        .stream
        .request
        .max_retry_delay_ms
        .unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS as f64);
    if maximum > 0.0 && delay_ms as f64 > maximum {
        return Err(CodexRunError::retry_delay(format!(
            "Server requested {}s retry delay (max: {}s)",
            delay_ms.div_ceil(1_000),
            (maximum / 1_000.0).ceil()
        )));
    }
    Ok(delay_ms)
}

fn normalize_timeout_ms(value: Option<f64>) -> Result<Option<u64>, CodexRunError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if !value.is_finite() || value < 0.0 || value.floor() > u64::MAX as f64 {
        return Err(CodexRunError::new(format!(
            "Invalid timeoutMs: {}",
            crate::utils::error_body::js_f64_string(value)
        )));
    }
    Ok(Some(value.floor() as u64))
}

fn parse_error_response(status: u16, status_text: &str, raw: &str) -> String {
    let mut message = if !raw.is_empty() {
        raw.to_owned()
    } else if !status_text.is_empty() {
        status_text.to_owned()
    } else {
        "Request failed".to_owned()
    };
    if let Ok(value) = serde_json::from_str::<Value>(raw)
        && let Some(error) = value.get("error").filter(|error| js_truthy(error))
    {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .or_else(|| error.get("type").and_then(Value::as_str))
            .unwrap_or_default();
        let usage_limit = [
            "usage_limit_reached",
            "usage_not_included",
            "rate_limit_exceeded",
        ]
        .iter()
        .any(|pattern| code.to_ascii_lowercase().contains(pattern))
            || status == 429;
        let friendly = usage_limit.then(|| {
            let plan = error
                .get("plan_type")
                .and_then(Value::as_str)
                .filter(|plan| !plan.is_empty())
                .map(|plan| format!(" ({} plan)", plan.to_ascii_lowercase()))
                .unwrap_or_default();
            let when = error
                .get("resets_at")
                .and_then(Value::as_f64)
                .filter(|reset| *reset != 0.0)
                .and_then(|reset| {
                    let now_ms = now_millis().to_string().parse::<f64>().ok()?;
                    let minutes = ((reset * 1_000.0 - now_ms) / 60_000.0).round().max(0.0);
                    format!("{minutes:.0}").parse::<u64>().ok()
                })
                .map(|minutes| format!(" Try again in ~{minutes} min."))
                .unwrap_or_default();
            format!("You have hit your ChatGPT usage limit{plan}.{when}")
                .trim()
                .to_owned()
        });
        message = friendly
            .or_else(|| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or(message);
    }
    message
}

#[cfg(test)]
mod tests;
