use crate::api::google_shared::{
    GoogleApiThinkingLevel, GoogleSdkError, ResolvedGoogleThinkingLevel, convert_messages,
    convert_tools, is_thinking_part, map_stop_reason_string, resolve_google_function_calling_mode,
    resolve_google_thinking_level, retain_thought_signature, retry_google_request,
    supports_google_strict_tool_sampling,
};
use crate::api::simple_options::build_base_options;
use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::event_stream::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantStreamSender,
};
use crate::models::{calculate_cost, clamp_thinking_level};
use crate::types::{
    AssistantContent, Context, ErrorStopReason, Model, ModelThinkingLevel, SimpleStreamOptions,
    StopReason, StreamOptions, SuccessfulStopReason, TextContent, ThinkingBudgets, ThinkingContent,
    ToolCall, ToolChoice, Usage, is_default_fetch,
};
use crate::utils::pi_user_agent::get_pi_user_agent;
use crate::utils::provider_retry::ProviderRetryOptions;
use crate::utils::sanitize_unicode::sanitize_surrogates;
use eventsource_stream::Eventsource;
use futures::{StreamExt, TryStreamExt};
use google_cloud_auth::credentials::{CacheableResource, Credentials};
use reqwest_012::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoogleToolChoice {
    #[default]
    Auto,
    None,
    Any,
}

impl GoogleToolChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Any => "any",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleThinkingOptions {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<GoogleApiThinkingLevel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<GoogleToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<GoogleThinkingOptions>,
}

#[derive(Debug, Clone, Default)]
pub struct GoogleGenerativeAIApi;

pub fn google_generative_ai_api() -> GoogleGenerativeAIApi {
    GoogleGenerativeAIApi
}

impl ProviderStreams for GoogleGenerativeAIApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        let options = match options {
            ApiStreamOptions::Base(stream) => GoogleOptions {
                stream,
                ..GoogleOptions::default()
            },
            ApiStreamOptions::GoogleGenerativeAI(options) => options,
            ApiStreamOptions::Custom { base, extra } => {
                let mut value = serde_json::to_value(base).unwrap_or(Value::Object(Map::new()));
                if let Some(object) = value.as_object_mut() {
                    object.extend(extra);
                }
                match serde_json::from_value(value) {
                    Ok(options) => options,
                    Err(error) => return setup_error_stream(model, error.to_string(), false),
                }
            }
            _ => {
                return setup_error_stream(
                    model,
                    "Google Generative AI received options for a different API".to_owned(),
                    false,
                );
            }
        };
        stream(model, context, options)
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

pub fn stream(
    model: &Model,
    context: &Context,
    options: GoogleOptions,
) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::channel();
    let model = model.clone();
    let context = context.clone();
    tokio::spawn(async move {
        let mut output = crate::types::AssistantMessage::pending(
            "google-generative-ai",
            model.provider.clone(),
            model.id.clone(),
            now_millis(),
        );
        if let Err(error) = run_stream(&sender, &model, &context, &options, &mut output).await {
            output.stop_reason = if options
                .stream
                .request
                .signal
                .as_ref()
                .is_some_and(|signal| signal.is_aborted())
            {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            output.error_message = Some(error);
            let reason = if output.stop_reason == StopReason::Aborted {
                ErrorStopReason::Aborted
            } else {
                ErrorStopReason::Error
            };
            let _ = sender.send(AssistantMessageEvent::Error {
                reason,
                error: output,
            });
        }
    });
    stream
}

async fn run_stream(
    sender: &AssistantStreamSender,
    model: &Model,
    context: &Context,
    options: &GoogleOptions,
    output: &mut crate::types::AssistantMessage,
) -> Result<(), String> {
    if options
        .stream
        .request
        .fetch
        .as_ref()
        .is_some_and(|fetch| !is_default_fetch(fetch))
    {
        return Err("Custom fetch is not supported by the Google Generative AI adapter".to_owned());
    }
    let api_key = options
        .stream
        .request
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| format!("No API key for provider: {}", model.provider))?;
    let backend = create_studio_backend(model, api_key, options)?;
    let mut params = build_params(model, context, options)?;
    if let Some(on_payload) = &options.stream.request.on_payload
        && let Some(replacement) = on_payload(params.clone(), model).await?
    {
        params = replacement;
    }
    let request = google_wire_request_from_params(&params, &backend.target)?;
    let retry = ProviderRetryOptions {
        max_retries: options.stream.request.max_retries,
        max_retry_delay_ms: options.stream.request.max_retry_delay_ms,
        signal: options.stream.request.signal.clone(),
    };
    let google_stream = retry_google_request(
        || {
            let request = request.clone();
            start_google_stream(&backend, request, options.stream.request.signal.as_ref())
        },
        retry,
    )
    .await
    .map_err(|error| error.to_string())?;

    consume_google_stream(
        sender,
        model,
        &options.stream,
        output,
        google_stream,
        &TOOL_CALL_COUNTER,
        "Google stream ended without a finish reason",
    )
    .await
}

pub(crate) async fn consume_google_stream(
    sender: &AssistantStreamSender,
    model: &Model,
    options: &StreamOptions,
    output: &mut crate::types::AssistantMessage,
    mut google_stream: adk_gemini::backend::BackendStream<Value>,
    tool_call_counter: &AtomicU64,
    missing_finish_message: &str,
) -> Result<(), String> {
    sender
        .send(AssistantMessageEvent::Start)
        .map_err(|error| error.to_string())?;
    let mut current = None;
    loop {
        let item = if let Some(signal) = options.request.signal.as_ref() {
            tokio::select! {
                biased;
                _ = signal.cancelled() => return Err("Request was aborted".to_owned()),
                item = google_stream.next() => item,
            }
        } else {
            google_stream.next().await
        };
        let Some(chunk) = item else { break };
        let chunk = chunk.map_err(|error| error.to_string())?;
        process_chunk(
            sender,
            model,
            output,
            &chunk,
            &mut current,
            tool_call_counter,
        )?;
    }
    close_current_block(sender, output, &mut current)?;

    if options
        .request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err("Request was aborted".to_owned());
    }
    if output.stop_reason == StopReason::Pending {
        return Err(missing_finish_message.to_owned());
    }
    if matches!(output.stop_reason, StopReason::Aborted | StopReason::Error) {
        return Err(output.raw_stop_reason.as_ref().map_or_else(
            || "An unknown error occurred".to_owned(),
            |reason| format!("Provider stopped with: {reason}"),
        ));
    }
    let reason = successful_reason(output.stop_reason)?;
    sender
        .send(AssistantMessageEvent::Done {
            reason,
            message: output.clone(),
        })
        .map_err(|error| error.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentBlock {
    Text(usize),
    Thinking(usize),
}

fn process_chunk(
    sender: &AssistantStreamSender,
    model: &Model,
    output: &mut crate::types::AssistantMessage,
    chunk: &Value,
    current: &mut Option<CurrentBlock>,
    tool_call_counter: &AtomicU64,
) -> Result<(), String> {
    if output.response_id.as_deref().is_none_or(str::is_empty)
        && let Some(response_id) = chunk.get("responseId").and_then(Value::as_str)
    {
        output.response_id = Some(response_id.to_owned());
    }
    let candidate = chunk
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first());
    if let Some(parts) = candidate
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                let thinking = is_thinking_part(part);
                let wrong_kind = !matches!(
                    (*current, thinking),
                    (Some(CurrentBlock::Thinking(_)), true) | (Some(CurrentBlock::Text(_)), false)
                );
                if wrong_kind {
                    close_current_block(sender, output, current)?;
                    let index = output.content.len();
                    if thinking {
                        output
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent::new("")));
                        *current = Some(CurrentBlock::Thinking(index));
                        sender
                            .send(AssistantMessageEvent::ThinkingStart {
                                content_index: index,
                                thinking: None,
                                thinking_signature: None,
                                redacted: None,
                            })
                            .map_err(|error| error.to_string())?;
                    } else {
                        output
                            .content
                            .push(AssistantContent::Text(TextContent::new("")));
                        *current = Some(CurrentBlock::Text(index));
                        sender
                            .send(AssistantMessageEvent::TextStart {
                                content_index: index,
                            })
                            .map_err(|error| error.to_string())?;
                    }
                }
                let signature = part.get("thoughtSignature").and_then(Value::as_str);
                match current.expect("text established current block") {
                    CurrentBlock::Thinking(index) => {
                        let AssistantContent::Thinking(block) = &mut output.content[index] else {
                            unreachable!("current block kind is tracked")
                        };
                        block.thinking.push_str(text);
                        block.thinking_signature = retain_thought_signature(
                            block.thinking_signature.as_deref(),
                            signature,
                        );
                        sender
                            .send(AssistantMessageEvent::ThinkingDelta {
                                content_index: index,
                                delta: text.to_owned(),
                                thinking_signature_delta: None,
                            })
                            .map_err(|error| error.to_string())?;
                    }
                    CurrentBlock::Text(index) => {
                        let AssistantContent::Text(block) = &mut output.content[index] else {
                            unreachable!("current block kind is tracked")
                        };
                        block.text.push_str(text);
                        block.text_signature =
                            retain_thought_signature(block.text_signature.as_deref(), signature);
                        sender
                            .send(AssistantMessageEvent::TextDelta {
                                content_index: index,
                                delta: text.to_owned(),
                            })
                            .map_err(|error| error.to_string())?;
                    }
                }
            }
            if let Some(function_call) = part.get("functionCall").and_then(Value::as_object) {
                close_current_block(sender, output, current)?;
                let name = function_call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let provided_id = function_call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty());
                let duplicate = provided_id.is_some_and(|id| {
                    output.content.iter().any(
                        |block| matches!(block, AssistantContent::ToolCall(call) if call.id == id),
                    )
                });
                let id = match provided_id {
                    Some(id) if !duplicate => id.to_owned(),
                    _ => format!(
                        "{name}_{}_{}",
                        now_millis(),
                        tool_call_counter.fetch_add(1, Ordering::Relaxed) + 1
                    ),
                };
                let arguments = function_call
                    .get("args")
                    .filter(|value| !value.is_null())
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                let mut tool_call = ToolCall::new(id, name, arguments);
                tool_call.thought_signature = part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .filter(|signature| !signature.is_empty())
                    .map(str::to_owned);
                let index = output.content.len();
                output
                    .content
                    .push(AssistantContent::ToolCall(tool_call.clone()));
                sender
                    .send(AssistantMessageEvent::ToolCallStart {
                        content_index: index,
                        id: tool_call.id.clone(),
                        tool_name: tool_call.name.clone(),
                        namespace: None,
                    })
                    .map_err(|error| error.to_string())?;
                sender
                    .send(AssistantMessageEvent::ToolCallDelta {
                        content_index: index,
                        delta: serde_json::to_string(&tool_call.arguments)
                            .map_err(|error| error.to_string())?,
                    })
                    .map_err(|error| error.to_string())?;
                sender
                    .send(AssistantMessageEvent::ToolCallEnd {
                        content_index: index,
                        tool_call,
                    })
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    if let Some(reason) = candidate
        .and_then(|candidate| candidate.get("finishReason"))
        .and_then(|value| value.as_str())
        .filter(|reason| !reason.is_empty())
    {
        output.raw_stop_reason = Some(reason.to_owned());
        output.stop_reason = stream_stop_reason(reason)?;
        if output.stop_reason == StopReason::Stop
            && output
                .content
                .iter()
                .any(|block| matches!(block, AssistantContent::ToolCall(_)))
        {
            output.stop_reason = StopReason::ToolUse;
        }
    }
    if let Some(usage) = chunk.get("usageMetadata") {
        let prompt = number_field(usage, "promptTokenCount");
        let cached = number_field(usage, "cachedContentTokenCount");
        let candidates = number_field(usage, "candidatesTokenCount");
        let thoughts = number_field(usage, "thoughtsTokenCount");
        output.usage = Usage {
            input: (prompt - cached).into(),
            output: (candidates + thoughts).into(),
            cache_read: cached.into(),
            cache_write: 0.into(),
            cache_write_1h: None,
            reasoning: Some(thoughts.into()),
            total_tokens: number_field(usage, "totalTokenCount").into(),
            cost: Default::default(),
        };
        calculate_cost(model, &mut output.usage);
    }
    Ok(())
}

fn stream_stop_reason(reason: &str) -> Result<StopReason, String> {
    match reason {
        "STOP" | "MAX_TOKENS" => Ok(map_stop_reason_string(reason)),
        "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "SAFETY"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "IMAGE_OTHER"
        | "RECITATION"
        | "FINISH_REASON_UNSPECIFIED"
        | "OTHER"
        | "LANGUAGE"
        | "MALFORMED_FUNCTION_CALL"
        | "UNEXPECTED_TOOL_CALL"
        | "NO_IMAGE" => Ok(StopReason::Error),
        _ => Err(format!("Unhandled stop reason: {reason}")),
    }
}

fn close_current_block(
    sender: &AssistantStreamSender,
    output: &crate::types::AssistantMessage,
    current: &mut Option<CurrentBlock>,
) -> Result<(), String> {
    match current.take() {
        Some(CurrentBlock::Text(index)) => {
            let AssistantContent::Text(block) = &output.content[index] else {
                unreachable!("current block kind is tracked")
            };
            sender
                .send(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: block.text.clone(),
                    content_signature: None,
                })
                .map_err(|error| error.to_string())
        }
        Some(CurrentBlock::Thinking(index)) => {
            let AssistantContent::Thinking(block) = &output.content[index] else {
                unreachable!("current block kind is tracked")
            };
            sender
                .send(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: block.thinking.clone(),
                    content_signature: None,
                    redacted: None,
                })
                .map_err(|error| error.to_string())
        }
        None => Ok(()),
    }
}

fn number_field(value: &Value, name: &str) -> f64 {
    value.get(name).and_then(Value::as_f64).unwrap_or(0.0)
}

fn successful_reason(reason: StopReason) -> Result<SuccessfulStopReason, String> {
    match reason {
        StopReason::Stop => Ok(SuccessfulStopReason::Stop),
        StopReason::Length => Ok(SuccessfulStopReason::Length),
        StopReason::ToolUse => Ok(SuccessfulStopReason::ToolUse),
        StopReason::Deferred => Ok(SuccessfulStopReason::Deferred),
        _ => Err("Google stream did not finish successfully".to_owned()),
    }
}

pub(crate) fn create_studio_backend(
    model: &Model,
    api_key: &str,
    options: &GoogleOptions,
) -> Result<GoogleBackend, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-goog-api-key"),
        HeaderValue::from_str(api_key).map_err(|error| error.to_string())?,
    );
    let (base_url, api_version) = if model.base_url.is_empty() {
        (
            "https://generativelanguage.googleapis.com".to_owned(),
            "v1beta".to_owned(),
        )
    } else {
        (model.base_url.clone(), String::new())
    };
    create_google_backend(
        model,
        headers,
        &options.stream,
        base_url,
        api_version,
        false,
        GoogleRequestTarget::mldev(),
    )
}

pub(crate) fn create_google_backend(
    model: &Model,
    headers: HeaderMap,
    options: &StreamOptions,
    base_url: String,
    api_version: String,
    collection_scope: bool,
    target: GoogleRequestTarget,
) -> Result<GoogleBackend, String> {
    let merged_headers = merged_google_client_headers(model, headers, options)?;
    let client = reqwest_012::Client::builder()
        .default_headers(merged_headers)
        .build()
        .map_err(|error| error.to_string())?;
    Url::parse(&base_url).map_err(|error| error.to_string())?;
    Ok(GoogleBackend {
        client,
        base_url,
        api_version,
        collection_scope,
        credentials: None,
        target,
    })
}

fn merged_google_client_headers(
    model: &Model,
    headers: HeaderMap,
    options: &StreamOptions,
) -> Result<HeaderMap, String> {
    let mut merged_headers = merged_google_headers(model, HeaderMap::new(), options)?;
    for (name, value) in &headers {
        if name == HeaderName::from_static("x-goog-api-key") {
            if !merged_headers.contains_key(name) {
                merged_headers.insert(name.clone(), value.clone());
            }
        } else {
            merged_headers.insert(name.clone(), value.clone());
        }
    }
    Ok(merged_headers)
}

pub(crate) fn merged_google_headers(
    model: &Model,
    mut headers: HeaderMap,
    options: &StreamOptions,
) -> Result<HeaderMap, String> {
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&get_pi_user_agent()).map_err(|error| error.to_string())?,
    );
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
            let value = HeaderValue::from_str(value).map_err(|error| error.to_string())?;
            headers.insert(name, value);
        }
    }
    if let Some(option_headers) = &options.request.headers {
        for (name, value) in option_headers {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
            if let Some(value) = value {
                headers.insert(
                    name,
                    HeaderValue::from_str(value).map_err(|error| error.to_string())?,
                );
            } else {
                headers.remove(name);
            }
        }
    }
    Ok(headers)
}

#[derive(Debug, Clone)]
pub(crate) struct GoogleRequestTarget {
    vertex: bool,
    project: Option<String>,
    location: Option<String>,
}

impl GoogleRequestTarget {
    pub(crate) fn mldev() -> Self {
        Self {
            vertex: false,
            project: None,
            location: None,
        }
    }

    pub(crate) fn vertex(project: Option<String>, location: Option<String>) -> Self {
        Self {
            vertex: true,
            project,
            location,
        }
    }
}

#[derive(Debug)]
pub(crate) struct GoogleBackend {
    client: reqwest_012::Client,
    base_url: String,
    api_version: String,
    collection_scope: bool,
    credentials: Option<Credentials>,
    pub(crate) target: GoogleRequestTarget,
}

#[derive(Debug, Clone)]
pub(crate) struct GoogleWireRequest {
    model: String,
    body: Value,
    http_options: GoogleHttpOptions,
}

#[derive(Debug, Clone, Default)]
struct GoogleHttpOptions {
    base_url: Option<String>,
    api_version: Option<String>,
    collection_scope: Option<bool>,
    headers: Map<String, Value>,
    timeout_ms: Option<f64>,
    extra_body: Option<Value>,
}

impl GoogleBackend {
    pub(crate) fn with_credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    async fn generate_content_stream(
        &self,
        request: GoogleWireRequest,
    ) -> Result<adk_gemini::backend::BackendStream<Value>, adk_gemini::ClientError> {
        let url = self.request_url(&request)?;
        let mut body = request.body;
        if let Some(extra_body) = request.http_options.extra_body.as_ref() {
            deep_merge_json(&mut body, extra_body);
        }
        let mut builder = self.client.post(url).json(&body);
        for (name, value) in &request.http_options.headers {
            let Some(value) = value.as_str() else {
                continue;
            };
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                adk_gemini::ClientError::BadResponse {
                    code: 400,
                    description: Some(error.to_string()),
                }
            })?;
            let value = HeaderValue::from_str(value).map_err(|error| {
                adk_gemini::ClientError::BadResponse {
                    code: 400,
                    description: Some(error.to_string()),
                }
            })?;
            builder = builder.header(name, value);
        }
        if let Some(credentials) = &self.credentials {
            let headers = credentials
                .headers(http::Extensions::new())
                .await
                .map_err(|error| adk_gemini::ClientError::BadResponse {
                    code: 401,
                    description: Some(error.to_string()),
                })?;
            let CacheableResource::New { data, .. } = headers else {
                unreachable!("no entity tag was supplied")
            };
            for (name, value) in data {
                if let Some(name) = name {
                    builder = builder.header(name, value);
                }
            }
        }
        if let Some(timeout_ms) = request
            .http_options
            .timeout_ms
            .filter(|timeout| timeout.is_finite() && *timeout > 0.0)
        {
            builder = builder
                .timeout(Duration::from_secs_f64(timeout_ms / 1_000.0))
                .header(
                    "x-server-timeout",
                    (timeout_ms / 1_000.0).ceil().to_string(),
                );
        }
        let response = builder
            .send()
            .await
            .map_err(|source| adk_gemini::ClientError::PerformRequestNew { source })?;
        let status = response.status();
        if !status.is_success() {
            return Err(adk_gemini::ClientError::BadResponse {
                code: status.as_u16(),
                description: response.text().await.ok(),
            });
        }
        let stream = response
            .bytes_stream()
            .eventsource()
            .map_err(|source| adk_gemini::ClientError::BadPart { source })
            .and_then(|event| async move { decode_google_stream_event(&event.data) });
        Ok(Box::pin(stream))
    }

    fn request_url(&self, request: &GoogleWireRequest) -> Result<Url, adk_gemini::ClientError> {
        let base_url = request
            .http_options
            .base_url
            .as_deref()
            .unwrap_or(&self.base_url)
            .trim_end_matches('/');
        let api_version = request
            .http_options
            .api_version
            .as_deref()
            .unwrap_or(&self.api_version);
        let collection_scope = request
            .http_options
            .collection_scope
            .unwrap_or(self.collection_scope);
        let mut url = base_url.to_owned();
        if !api_version.is_empty() {
            url.push('/');
            url.push_str(api_version.trim_matches('/'));
        }
        if self.target.vertex
            && !collection_scope
            && !request.model.starts_with("projects/")
            && let (Some(project), Some(location)) = (&self.target.project, &self.target.location)
        {
            url.push_str("/projects/");
            url.push_str(project);
            url.push_str("/locations/");
            url.push_str(location);
        }
        url.push('/');
        url.push_str(&request.model);
        url.push_str(":streamGenerateContent");
        let mut url = Url::parse(&url).map_err(|source| adk_gemini::ClientError::ConstructUrl {
            source,
            suffix: "streamGenerateContent".to_owned(),
        })?;
        url.query_pairs_mut().append_pair("alt", "sse");
        Ok(url)
    }
}

fn decode_google_stream_event(data: &str) -> Result<Value, adk_gemini::ClientError> {
    serde_json::from_str::<Value>(data)
        .map_err(|source| adk_gemini::ClientError::Deserialize { source })
}

pub(crate) fn google_wire_request_from_params(
    params: &Value,
    target: &GoogleRequestTarget,
) -> Result<GoogleWireRequest, String> {
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "model is required and must be a string".to_owned())?;
    let model = transform_model(model, target.vertex)?;
    let mut body = Map::new();
    if let Some(contents) = params.get("contents").filter(|value| !value.is_null()) {
        body.insert(
            "contents".to_owned(),
            transform_contents(contents, target.vertex)?,
        );
    }
    let mut http_options = GoogleHttpOptions::default();
    if let Some(config) = params.get("config").filter(|value| !value.is_null()) {
        let mut config = config.as_object().cloned().unwrap_or_default();
        if config
            .get("responseJsonSchema")
            .is_none_or(|value| !js_truthy(value))
            && config
                .get("responseSchema")
                .and_then(Value::as_object)
                .is_some_and(|schema| schema.contains_key("$schema"))
            && let Some(schema) = config.remove("responseSchema")
        {
            config.insert("responseJsonSchema".to_owned(), schema);
        }
        http_options = parse_google_http_options(config.get("httpOptions"));
        let generation_config = transform_generation_config(&config, &mut body, target)?;
        body.insert("generationConfig".to_owned(), generation_config);
    }
    Ok(GoogleWireRequest {
        model,
        body: Value::Object(body),
        http_options,
    })
}

fn transform_model(model: &str, vertex: bool) -> Result<String, String> {
    if model.is_empty() {
        return Err("model is required and must be a string".to_owned());
    }
    if model.contains("..") || model.contains('?') || model.contains('&') {
        return Err("invalid model parameter".to_owned());
    }
    if vertex {
        if model.starts_with("publishers/")
            || model.starts_with("projects/")
            || model.starts_with("models/")
        {
            Ok(model.to_owned())
        } else if let Some((publisher, name)) = model.split_once('/') {
            Ok(format!("publishers/{publisher}/models/{name}"))
        } else {
            Ok(format!("publishers/google/models/{model}"))
        }
    } else if model.starts_with("models/") || model.starts_with("tunedModels/") {
        Ok(model.to_owned())
    } else {
        Ok(format!("models/{model}"))
    }
}

fn transform_contents(contents: &Value, vertex: bool) -> Result<Value, String> {
    if contents.is_null() || contents.as_array().is_some_and(Vec::is_empty) {
        return Err("contents are required".to_owned());
    }
    let normalized = if let Some(items) = contents.as_array() {
        let first_is_content = items.first().is_some_and(is_content);
        let mut normalized = Vec::new();
        let mut parts = Vec::new();
        for item in items {
            if is_content(item) != first_is_content {
                return Err("Mixing Content and Parts is not supported, please group the parts into a the appropriate Content objects and specify the roles for them".to_owned());
            }
            if first_is_content {
                normalized.push(item.clone());
            } else {
                reject_bare_function_part(item)?;
                parts.push(transform_part_union(item)?);
            }
        }
        if !first_is_content {
            normalized.push(json!({ "role": "user", "parts": parts }));
        }
        normalized
    } else {
        reject_bare_function_part(contents)?;
        vec![transform_content_union(contents)?]
    };
    normalized
        .iter()
        .map(|content| transform_content(content, vertex))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

fn is_content(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|content| content.get("parts"))
        .is_some_and(Value::is_array)
}

fn reject_bare_function_part(value: &Value) -> Result<(), String> {
    if value.as_object().is_some_and(|part| {
        part.contains_key("functionCall") || part.contains_key("functionResponse")
    }) {
        Err("To specify functionCall or functionResponse parts, please wrap them in a Content object, specifying the role for them".to_owned())
    } else {
        Ok(())
    }
}

fn transform_content_union(value: &Value) -> Result<Value, String> {
    if value.is_null() {
        return Err("ContentUnion is required".to_owned());
    }
    if is_content(value) {
        return Ok(value.clone());
    }
    Ok(json!({ "role": "user", "parts": [transform_part_union(value)?] }))
}

fn transform_part_union(value: &Value) -> Result<Value, String> {
    match value {
        Value::Null => Err("PartUnion is required".to_owned()),
        Value::String(text) => Ok(json!({ "text": text })),
        Value::Object(_) | Value::Array(_) => Ok(value.clone()),
        other => Err(format!(
            "Unsupported part type: {}",
            match other {
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                _ => "unknown",
            }
        )),
    }
}

fn transform_content(value: &Value, vertex: bool) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    let mut content = Map::new();
    if let Some(parts) = object.get("parts").and_then(Value::as_array) {
        content.insert(
            "parts".to_owned(),
            parts
                .iter()
                .map(|part| transform_part(part, vertex))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        );
    }
    copy_non_null(&object, &mut content, "role");
    Ok(Value::Object(content))
}

fn transform_part(value: &Value, vertex: bool) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    if vertex {
        for name in ["toolCall", "toolResponse", "partMetadata"] {
            if object.contains_key(name) {
                return Err(format!(
                    "{name} parameter is not supported in Gemini Enterprise Agent Platform (previously known as Vertex AI)."
                ));
            }
        }
    }
    let mut part = Map::new();
    for name in [
        "mediaResolution",
        "codeExecutionResult",
        "executableCode",
        "functionResponse",
        "text",
        "thought",
        "thoughtSignature",
        "videoMetadata",
        "toolCall",
        "toolResponse",
        "partMetadata",
    ] {
        copy_non_null(&object, &mut part, name);
    }
    if let Some(file_data) = object.get("fileData").filter(|value| !value.is_null()) {
        part.insert(
            "fileData".to_owned(),
            if vertex {
                file_data.clone()
            } else {
                transform_mldev_file_data(file_data)?
            },
        );
    }
    if let Some(call) = object.get("functionCall").filter(|value| !value.is_null()) {
        part.insert(
            "functionCall".to_owned(),
            if vertex {
                call.clone()
            } else {
                transform_mldev_function_call(call)?
            },
        );
    }
    if let Some(data) = object.get("inlineData").filter(|value| !value.is_null()) {
        part.insert(
            "inlineData".to_owned(),
            if vertex {
                data.clone()
            } else {
                transform_mldev_inline_data(data)?
            },
        );
    }
    Ok(Value::Object(part))
}

fn transform_mldev_file_data(value: &Value) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    reject_present(&object, "displayName", "Gemini API")?;
    Ok(Value::Object(copy_fields(
        &object,
        &["fileUri", "mimeType"],
    )))
}

fn transform_mldev_inline_data(value: &Value) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    reject_present(&object, "displayName", "Gemini API")?;
    Ok(Value::Object(copy_fields(&object, &["data", "mimeType"])))
}

fn transform_mldev_function_call(value: &Value) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    reject_present(&object, "partialArgs", "Gemini API")?;
    reject_present(&object, "willContinue", "Gemini API")?;
    Ok(Value::Object(copy_fields(&object, &["id", "args", "name"])))
}

fn transform_generation_config(
    config: &Map<String, Value>,
    body: &mut Map<String, Value>,
    target: &GoogleRequestTarget,
) -> Result<Value, String> {
    let mut generation = copy_fields(
        config,
        &[
            "temperature",
            "topP",
            "topK",
            "candidateCount",
            "maxOutputTokens",
            "stopSequences",
            "responseLogprobs",
            "logprobs",
            "presencePenalty",
            "frequencyPenalty",
            "seed",
            "responseMimeType",
            "responseJsonSchema",
            "responseModalities",
            "mediaResolution",
            "thinkingConfig",
        ],
    );
    if let Some(system) = config
        .get("systemInstruction")
        .filter(|value| !value.is_null())
    {
        body.insert(
            "systemInstruction".to_owned(),
            transform_content(&transform_content_union(system)?, target.vertex)?,
        );
    }
    if let Some(schema) = config
        .get("responseSchema")
        .filter(|value| !value.is_null())
    {
        generation.insert("responseSchema".to_owned(), process_json_schema(schema)?);
    }
    if target.vertex {
        copy_non_null(config, &mut generation, "routingConfig");
        if let Some(value) = config
            .get("modelSelectionConfig")
            .filter(|value| !value.is_null())
        {
            generation.insert("modelConfig".to_owned(), value.clone());
        }
        if config.contains_key("enableEnhancedCivicAnswers") {
            return Err("enableEnhancedCivicAnswers parameter is not supported in Gemini Enterprise Agent Platform (previously known as Vertex AI).".to_owned());
        }
    } else {
        reject_present(config, "routingConfig", "Gemini API")?;
        reject_present(config, "modelSelectionConfig", "Gemini API")?;
        reject_present(config, "labels", "Gemini API")?;
        reject_present(config, "audioTimestamp", "Gemini API")?;
        reject_present(config, "modelArmorConfig", "Gemini API")?;
        copy_non_null(config, &mut generation, "enableEnhancedCivicAnswers");
    }
    if let Some(settings) = config
        .get("safetySettings")
        .filter(|value| !value.is_null())
    {
        body.insert(
            "safetySettings".to_owned(),
            if target.vertex {
                settings.clone()
            } else {
                Value::Array(
                    settings
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(transform_mldev_safety_setting)
                        .collect::<Result<Vec<_>, _>>()?,
                )
            },
        );
    }
    if let Some(tools) = config.get("tools").filter(|value| !value.is_null()) {
        let tools = tools
            .as_array()
            .ok_or_else(|| "tools is required and must be an array of Tools".to_owned())?;
        body.insert(
            "tools".to_owned(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| transform_tool(tool, target.vertex))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        );
    }
    if let Some(tool_config) = config.get("toolConfig").filter(|value| !value.is_null()) {
        body.insert(
            "toolConfig".to_owned(),
            transform_tool_config(tool_config, target.vertex)?,
        );
    }
    for name in ["labels", "modelArmorConfig", "serviceTier"] {
        if target.vertex || name == "serviceTier" {
            copy_non_null(config, body, name);
        }
    }
    if let Some(cached) = config.get("cachedContent").filter(|value| !value.is_null()) {
        body.insert(
            "cachedContent".to_owned(),
            Value::String(transform_cached_content(cached, target)?),
        );
    }
    if let Some(speech) = config.get("speechConfig").filter(|value| !value.is_null()) {
        generation.insert("speechConfig".to_owned(), transform_speech_config(speech)?);
    }
    copy_non_null(config, &mut generation, "audioTimestamp");
    if let Some(image) = config.get("imageConfig").filter(|value| !value.is_null()) {
        generation.insert(
            "imageConfig".to_owned(),
            transform_image_config(image, target.vertex)?,
        );
    }
    Ok(Value::Object(generation))
}

fn transform_cached_content(value: &Value, target: &GoogleRequestTarget) -> Result<String, String> {
    let name = value
        .as_str()
        .ok_or_else(|| "name must be a string".to_owned())?;
    let should_append = !name.starts_with("cachedContents/") && !name.contains('/');
    if !target.vertex {
        return Ok(if should_append {
            format!("cachedContents/{name}")
        } else {
            name.to_owned()
        });
    }
    if name.starts_with("projects/") {
        Ok(name.to_owned())
    } else if name.starts_with("locations/") {
        Ok(format!(
            "projects/{}/{name}",
            target.project.as_deref().unwrap_or("undefined")
        ))
    } else if name.starts_with("cachedContents/") || should_append {
        Ok(format!(
            "projects/{}/locations/{}/{}{}",
            target.project.as_deref().unwrap_or("undefined"),
            target.location.as_deref().unwrap_or("undefined"),
            if should_append { "cachedContents/" } else { "" },
            name
        ))
    } else {
        Ok(name.to_owned())
    }
}

fn transform_tool(value: &Value, vertex: bool) -> Result<Value, String> {
    let mut object = value.as_object().cloned().unwrap_or_default();
    if let Some(declarations) = object
        .get_mut("functionDeclarations")
        .and_then(Value::as_array_mut)
    {
        for declaration in declarations {
            transform_function_declaration_schemas(declaration)?;
        }
    }
    if vertex {
        for name in ["fileSearch", "mcpServers"] {
            if object.contains_key(name) {
                return Err(format!(
                    "{name} parameter is not supported in Gemini Enterprise Agent Platform (previously known as Vertex AI)."
                ));
            }
        }
        let mut tool = copy_fields(
            &object,
            &[
                "retrieval",
                "computerUse",
                "googleSearch",
                "googleMaps",
                "codeExecution",
                "enterpriseWebSearch",
                "googleSearchRetrieval",
                "parallelAiSearch",
                "urlContext",
            ],
        );
        if let Some(declarations) = object.get("functionDeclarations").and_then(Value::as_array) {
            tool.insert(
                "functionDeclarations".to_owned(),
                Value::Array(
                    declarations
                        .iter()
                        .map(transform_vertex_function_declaration)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            );
        }
        Ok(Value::Object(tool))
    } else {
        for name in ["retrieval", "enterpriseWebSearch", "parallelAiSearch"] {
            reject_present(&object, name, "Gemini API")?;
        }
        let mut tool = copy_fields(
            &object,
            &[
                "computerUse",
                "fileSearch",
                "codeExecution",
                "functionDeclarations",
                "googleSearchRetrieval",
                "urlContext",
                "mcpServers",
            ],
        );
        if let Some(search) = object.get("googleSearch").filter(|value| !value.is_null()) {
            tool.insert(
                "googleSearch".to_owned(),
                transform_mldev_google_search(search)?,
            );
        }
        if let Some(maps) = object.get("googleMaps").filter(|value| !value.is_null()) {
            tool.insert("googleMaps".to_owned(), transform_mldev_google_maps(maps)?);
        }
        Ok(Value::Object(tool))
    }
}

fn transform_mldev_google_search(value: &Value) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    reject_present(&object, "blockingConfidence", "Gemini API")?;
    reject_present(&object, "excludeDomains", "Gemini API")?;
    Ok(Value::Object(copy_fields(
        &object,
        &["searchTypes", "timeRangeFilter"],
    )))
}

fn transform_mldev_google_maps(value: &Value) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    let mut output = copy_fields(&object, &["enableWidget"]);
    if let Some(auth) = object.get("authConfig").filter(|value| !value.is_null()) {
        let auth = auth.as_object().cloned().unwrap_or_default();
        for name in [
            "apiKeyConfig",
            "authType",
            "googleServiceAccountConfig",
            "httpBasicAuthConfig",
            "oauthConfig",
            "oidcConfig",
        ] {
            reject_present(&auth, name, "Gemini API")?;
        }
        output.insert(
            "authConfig".to_owned(),
            Value::Object(copy_fields(&auth, &["apiKey"])),
        );
    }
    Ok(Value::Object(output))
}

fn transform_function_declaration_schemas(value: &mut Value) -> Result<(), String> {
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    for (schema_name, json_schema_name) in [
        ("parameters", "parametersJsonSchema"),
        ("response", "responseJsonSchema"),
    ] {
        let Some(schema) = object.get(schema_name).cloned() else {
            continue;
        };
        if schema
            .as_object()
            .is_some_and(|schema| schema.contains_key("$schema"))
        {
            if object
                .get(json_schema_name)
                .is_none_or(|value| !js_truthy(value))
            {
                object.insert(json_schema_name.to_owned(), schema);
                object.remove(schema_name);
            }
        } else {
            object.insert(schema_name.to_owned(), process_json_schema(&schema)?);
        }
    }
    Ok(())
}

fn transform_vertex_function_declaration(value: &Value) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    if object.contains_key("behavior") {
        return Err("behavior parameter is not supported in Gemini Enterprise Agent Platform (previously known as Vertex AI).".to_owned());
    }
    Ok(Value::Object(copy_fields(
        &object,
        &[
            "description",
            "name",
            "parameters",
            "parametersJsonSchema",
            "response",
            "responseJsonSchema",
        ],
    )))
}

fn transform_tool_config(value: &Value, vertex: bool) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    if vertex && object.contains_key("includeServerSideToolInvocations") {
        return Err("includeServerSideToolInvocations parameter is not supported in Gemini Enterprise Agent Platform (previously known as Vertex AI).".to_owned());
    }
    let mut output = copy_fields(&object, &["retrievalConfig"]);
    if let Some(function) = object
        .get("functionCallingConfig")
        .filter(|value| !value.is_null())
    {
        if vertex {
            output.insert("functionCallingConfig".to_owned(), function.clone());
        } else {
            let function = function.as_object().cloned().unwrap_or_default();
            reject_present(&function, "streamFunctionCallArguments", "Gemini API")?;
            output.insert(
                "functionCallingConfig".to_owned(),
                Value::Object(copy_fields(&function, &["allowedFunctionNames", "mode"])),
            );
        }
    }
    if !vertex {
        copy_non_null(&object, &mut output, "includeServerSideToolInvocations");
    }
    Ok(Value::Object(output))
}

fn transform_mldev_safety_setting(value: &Value) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    reject_present(&object, "method", "Gemini API")?;
    Ok(Value::Object(copy_fields(
        &object,
        &["category", "threshold"],
    )))
}

fn transform_speech_config(value: &Value) -> Result<Value, String> {
    match value {
        Value::Object(_) => Ok(value.clone()),
        Value::String(voice) => Ok(json!({
            "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": voice } }
        })),
        _ => Err("Unsupported speechConfig type".to_owned()),
    }
}

fn transform_image_config(value: &Value, vertex: bool) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    let mut output = copy_fields(&object, &["aspectRatio", "imageSize"]);
    if vertex {
        copy_non_null(&object, &mut output, "personGeneration");
        copy_non_null(&object, &mut output, "prominentPeople");
        let mut image_output = Map::new();
        if let Some(value) = object
            .get("outputMimeType")
            .filter(|value| !value.is_null())
        {
            image_output.insert("mimeType".to_owned(), value.clone());
        }
        if let Some(value) = object
            .get("outputCompressionQuality")
            .filter(|value| !value.is_null())
        {
            image_output.insert("compressionQuality".to_owned(), value.clone());
        }
        if let Some(value) = object
            .get("imageOutputOptions")
            .filter(|value| !value.is_null())
        {
            image_output = value.as_object().cloned().unwrap_or_default();
        }
        if !image_output.is_empty() {
            output.insert("imageOutputOptions".to_owned(), Value::Object(image_output));
        }
    } else {
        for name in [
            "personGeneration",
            "prominentPeople",
            "outputMimeType",
            "outputCompressionQuality",
            "imageOutputOptions",
        ] {
            reject_present(&object, name, "Gemini API")?;
        }
    }
    Ok(Value::Object(output))
}

fn process_json_schema(value: &Value) -> Result<Value, String> {
    let object = value.as_object().cloned().unwrap_or_default();
    if object.get("type").is_some_and(|value| !value.is_null())
        && object.get("anyOf").is_some_and(|value| !value.is_null())
    {
        return Err("type and anyOf cannot be both populated.".to_owned());
    }
    let mut source = object.clone();
    let mut output = Map::new();
    if let Some(any_of) = object.get("anyOf").and_then(Value::as_array)
        && any_of.len() == 2
    {
        let null_index = any_of
            .iter()
            .position(|entry| entry.get("type").and_then(Value::as_str) == Some("null"));
        if let Some(index) = null_index {
            output.insert("nullable".to_owned(), Value::Bool(true));
            source = any_of[1 - index].as_object().cloned().unwrap_or_default();
        }
    }
    if let Some(types) = source.get("type").and_then(Value::as_array) {
        let mut non_null = types
            .iter()
            .filter_map(Value::as_str)
            .filter(|kind| *kind != "null")
            .collect::<Vec<_>>();
        if non_null.len() != types.len() {
            output.insert("nullable".to_owned(), Value::Bool(true));
        }
        if non_null.len() == 1 {
            output.insert(
                "type".to_owned(),
                Value::String(normalize_schema_type(non_null.remove(0))),
            );
        } else {
            output.insert(
                "anyOf".to_owned(),
                Value::Array(
                    non_null
                        .into_iter()
                        .map(|kind| json!({ "type": normalize_schema_type(kind) }))
                        .collect(),
                ),
            );
        }
    }
    for (name, value) in source {
        if value.is_null() {
            continue;
        }
        match name.as_str() {
            "type" if value.is_array() => {}
            "type" => {
                let kind = value.as_str().unwrap_or_default();
                if kind == "null" {
                    return Err(
                        "type: null can not be the only possible type for the field.".to_owned(),
                    );
                }
                output.insert(name, Value::String(normalize_schema_type(kind)));
            }
            "items" => {
                output.insert(name, process_json_schema(&value)?);
            }
            "anyOf" => {
                let mut any_of = Vec::new();
                for entry in value.as_array().cloned().unwrap_or_default() {
                    if entry.get("type").and_then(Value::as_str) == Some("null") {
                        output.insert("nullable".to_owned(), Value::Bool(true));
                    } else {
                        any_of.push(process_json_schema(&entry)?);
                    }
                }
                output.insert(name, Value::Array(any_of));
            }
            "properties" => {
                let mut properties = Map::new();
                for (property, schema) in value.as_object().cloned().unwrap_or_default() {
                    properties.insert(property, process_json_schema(&schema)?);
                }
                output.insert(name, Value::Object(properties));
            }
            "additionalProperties" => {}
            _ => {
                output.insert(name, value);
            }
        }
    }
    Ok(Value::Object(output))
}

fn normalize_schema_type(kind: &str) -> String {
    let kind = kind.to_ascii_uppercase();
    if matches!(
        kind.as_str(),
        "TYPE_UNSPECIFIED"
            | "STRING"
            | "NUMBER"
            | "INTEGER"
            | "BOOLEAN"
            | "ARRAY"
            | "OBJECT"
            | "NULL"
    ) {
        kind
    } else {
        "TYPE_UNSPECIFIED".to_owned()
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn parse_google_http_options(value: Option<&Value>) -> GoogleHttpOptions {
    let object = value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    GoogleHttpOptions {
        base_url: object
            .get("baseUrl")
            .and_then(Value::as_str)
            .map(str::to_owned),
        api_version: object
            .get("apiVersion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        collection_scope: object
            .get("baseUrlResourceScope")
            .and_then(Value::as_str)
            .map(|scope| scope == "COLLECTION"),
        headers: object
            .get("headers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        timeout_ms: object.get("timeout").and_then(Value::as_f64),
        extra_body: object
            .get("extraBody")
            .filter(|value| !value.is_null())
            .cloned(),
    }
}

fn deep_merge_json(target: &mut Value, source: &Value) {
    let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) else {
        return;
    };
    for (name, value) in source {
        if let Some(existing) = target.get_mut(name)
            && existing.is_object()
            && value.is_object()
        {
            deep_merge_json(existing, value);
        } else {
            target.insert(name.clone(), value.clone());
        }
    }
}

fn copy_fields(source: &Map<String, Value>, names: &[&str]) -> Map<String, Value> {
    let mut output = Map::new();
    for name in names {
        copy_non_null(source, &mut output, name);
    }
    output
}

fn copy_non_null(source: &Map<String, Value>, target: &mut Map<String, Value>, name: &str) {
    if let Some(value) = source.get(name).filter(|value| !value.is_null()) {
        target.insert(name.to_owned(), value.clone());
    }
}

fn reject_present(source: &Map<String, Value>, name: &str, endpoint: &str) -> Result<(), String> {
    if source.contains_key(name) {
        Err(format!("{name} parameter is not supported in {endpoint}."))
    } else {
        Ok(())
    }
}

pub(crate) fn build_params(
    model: &Model,
    context: &Context,
    options: &GoogleOptions,
) -> Result<Value, String> {
    if options
        .stream
        .request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err("Request aborted".to_owned());
    }
    let mut config = Map::new();
    if let Some(temperature) = options.stream.temperature {
        config.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(max_tokens) = options.stream.max_tokens {
        config.insert("maxOutputTokens".to_owned(), json!(max_tokens));
    }
    if let Some(system_prompt) = context
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
    {
        config.insert(
            "systemInstruction".to_owned(),
            Value::String(sanitize_surrogates(system_prompt)),
        );
    }
    let supports_strict = supports_google_strict_tool_sampling(&model.id);
    if let Some(tools) = context.tools.as_deref().filter(|tools| !tools.is_empty()) {
        if let Some(converted) = convert_tools(tools, false, supports_strict)? {
            config.insert("tools".to_owned(), Value::Array(converted));
        }
        if let Some(mode) = resolve_google_function_calling_mode(
            tools,
            options.tool_choice.map(GoogleToolChoice::as_str),
            supports_strict,
        )? {
            config.insert(
                "toolConfig".to_owned(),
                json!({ "functionCallingConfig": { "mode": mode } }),
            );
        }
    }
    if let Some(thinking) = &options.thinking {
        if thinking.enabled && model.reasoning {
            let mut thinking_config =
                Map::from_iter([("includeThoughts".to_owned(), Value::Bool(true))]);
            if let Some(level) = thinking.level {
                thinking_config.insert(
                    "thinkingLevel".to_owned(),
                    serde_json::to_value(level).map_err(|error| error.to_string())?,
                );
            } else if let Some(budget) = thinking.budget_tokens {
                thinking_config.insert("thinkingBudget".to_owned(), json!(budget));
            }
            config.insert("thinkingConfig".to_owned(), Value::Object(thinking_config));
        } else if model.reasoning && !thinking.enabled {
            config.insert("thinkingConfig".to_owned(), disabled_thinking_config(model));
        }
    }
    if options.stream.request.signal.is_some() {
        config.insert("abortSignal".to_owned(), json!({ "aborted": false }));
    }
    Ok(json!({
        "model": model.id,
        "contents": convert_messages(model, context),
        "config": config,
    }))
}

pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let api_key = options.stream.request.api_key.clone();
    if api_key.as_deref().is_none_or(str::is_empty) {
        return setup_error_stream(
            model,
            format!("No API key for provider: {}", model.provider),
            false,
        );
    }
    let mut base = GoogleOptions {
        stream: build_base_options(model, context, Some(&options), api_key.as_deref()),
        tool_choice: options.tool_choice.map(|choice| match choice {
            ToolChoice::Auto => GoogleToolChoice::Auto,
            ToolChoice::None => GoogleToolChoice::None,
        }),
        thinking: None,
    };
    let Some(reasoning) = options.reasoning else {
        base.thinking = Some(GoogleThinkingOptions {
            enabled: false,
            budget_tokens: None,
            level: None,
        });
        return stream(model, context, base);
    };
    let clamped = clamp_thinking_level(model, model_thinking_level(reasoning));
    let level = match resolve_google_thinking_level(model, clamped) {
        Ok(level) => level,
        Err(error) => return setup_error_stream(model, error, false),
    };
    base.thinking = if is_gemini_3_pro_model(model)
        || is_gemini_3_flash_model(model)
        || is_gemma_4_model(model)
    {
        Some(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: None,
            level: Some(thinking_level(level, model)),
        })
    } else {
        Some(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: Some(google_budget(
                model,
                level,
                options.thinking_budgets.as_ref(),
            )),
            level: None,
        })
    };
    stream(model, context, base)
}

fn model_thinking_level(level: crate::types::ThinkingLevel) -> ModelThinkingLevel {
    match level {
        crate::types::ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        crate::types::ThinkingLevel::Low => ModelThinkingLevel::Low,
        crate::types::ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        crate::types::ThinkingLevel::High => ModelThinkingLevel::High,
        crate::types::ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        crate::types::ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

fn is_gemma_4_model(model: &Model) -> bool {
    regex::Regex::new(r"gemma-?4")
        .expect("static regular expression")
        .is_match(&model.id.to_ascii_lowercase())
}

pub(crate) fn is_gemini_3_pro_model(model: &Model) -> bool {
    regex::Regex::new(r"gemini-3(?:\.\d+)?-pro")
        .expect("static regular expression")
        .is_match(&model.id.to_ascii_lowercase())
}

pub(crate) fn is_gemini_3_flash_model(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    regex::Regex::new(r"gemini-3(?:\.\d+)?-flash")
        .expect("static regular expression")
        .is_match(&id)
        || matches!(
            id.as_str(),
            "gemini-flash-latest" | "gemini-flash-lite-latest"
        )
}

fn disabled_thinking_config(model: &Model) -> Value {
    if is_gemini_3_pro_model(model) {
        json!({ "thinkingLevel": "LOW" })
    } else if is_gemini_3_flash_model(model) || is_gemma_4_model(model) {
        json!({ "thinkingLevel": "MINIMAL" })
    } else {
        json!({ "thinkingBudget": 0 })
    }
}

pub(crate) fn thinking_level(
    effort: ResolvedGoogleThinkingLevel,
    model: &Model,
) -> GoogleApiThinkingLevel {
    if is_gemini_3_pro_model(model) {
        return match effort {
            ResolvedGoogleThinkingLevel::Minimal | ResolvedGoogleThinkingLevel::Low => {
                GoogleApiThinkingLevel::Low
            }
            ResolvedGoogleThinkingLevel::Medium | ResolvedGoogleThinkingLevel::High => {
                GoogleApiThinkingLevel::High
            }
        };
    }
    if is_gemma_4_model(model) {
        return match effort {
            ResolvedGoogleThinkingLevel::Minimal | ResolvedGoogleThinkingLevel::Low => {
                GoogleApiThinkingLevel::Minimal
            }
            ResolvedGoogleThinkingLevel::Medium | ResolvedGoogleThinkingLevel::High => {
                GoogleApiThinkingLevel::High
            }
        };
    }
    match effort {
        ResolvedGoogleThinkingLevel::Minimal => GoogleApiThinkingLevel::Minimal,
        ResolvedGoogleThinkingLevel::Low => GoogleApiThinkingLevel::Low,
        ResolvedGoogleThinkingLevel::Medium => GoogleApiThinkingLevel::Medium,
        ResolvedGoogleThinkingLevel::High => GoogleApiThinkingLevel::High,
    }
}

pub(crate) fn google_budget(
    model: &Model,
    level: ResolvedGoogleThinkingLevel,
    custom: Option<&ThinkingBudgets>,
) -> f64 {
    let custom = custom.and_then(|budgets| match level {
        ResolvedGoogleThinkingLevel::Minimal => budgets.minimal,
        ResolvedGoogleThinkingLevel::Low => budgets.low,
        ResolvedGoogleThinkingLevel::Medium => budgets.medium,
        ResolvedGoogleThinkingLevel::High => budgets.high,
    });
    if let Some(custom) = custom {
        return custom;
    }
    let values = if model.id.contains("2.5-pro") {
        [128, 2_048, 8_192, 32_768]
    } else if model.id.contains("2.5-flash-lite") {
        [512, 2_048, 8_192, 24_576]
    } else if model.id.contains("2.5-flash") {
        [128, 2_048, 8_192, 24_576]
    } else {
        return -1.0;
    };
    f64::from(
        values[match level {
            ResolvedGoogleThinkingLevel::Minimal => 0,
            ResolvedGoogleThinkingLevel::Low => 1,
            ResolvedGoogleThinkingLevel::Medium => 2,
            ResolvedGoogleThinkingLevel::High => 3,
        }],
    )
}

pub(crate) async fn start_google_stream(
    backend: &GoogleBackend,
    request: GoogleWireRequest,
    signal: Option<&std::sync::Arc<dyn crate::types::AbortSignal>>,
) -> Result<adk_gemini::backend::BackendStream<Value>, GoogleSdkError> {
    if let Some(signal) = signal {
        tokio::select! {
            biased;
            _ = signal.cancelled() => Err(GoogleSdkError::aborted()),
            result = backend.generate_content_stream(request) => result.map_err(GoogleSdkError::new),
        }
    } else {
        backend
            .generate_content_stream(request)
            .await
            .map_err(GoogleSdkError::new)
    }
}

fn setup_error_stream(
    model: &Model,
    message: String,
    aborted: bool,
) -> AssistantMessageEventStream {
    let mut output = crate::types::AssistantMessage::pending(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_millis(),
    );
    output.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    output.error_message = Some(message);
    AssistantMessageEventStream::from_events(vec![AssistantMessageEvent::Error {
        reason: if aborted {
            ErrorStopReason::Aborted
        } else {
            ErrorStopReason::Error
        },
        error: output,
    }])
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
