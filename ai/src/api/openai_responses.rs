//! OpenAI Responses ⇐ pi `src/api/openai-responses.ts`.
//!
//! `openai-oxide` owns request issue and SSE framing. The raw Responses stream is used so
//! newly introduced event types remain forward-compatible; known events are decoded by
//! `openai_responses_shared`.

use crate::api::constrained_sampling::create_grammar_tool_input_properties;
use crate::api::github_copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use crate::api::openai_prompt_cache::clamp_open_ai_prompt_cache_key;
use crate::api::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ConvertResponsesToolsOptions, DeferredResponsesToolsMode,
    OpenAIResponsesError, OpenAIResponsesStreamOptions, ResponseReasoningSummary,
    ResponseServiceTier, ResponseTool, ResponseToolChoice, ResponseToolChoiceMode,
    convert_responses_messages, convert_responses_tools, process_responses_stream,
};
use crate::api::simple_options::build_base_options;
use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::event_stream::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantStreamSender,
};
use crate::models::clamp_thinking_level;
use crate::types::{
    AbortSignal, CacheRetention, Context, ErrorStopReason, Model, ModelCompat, ModelThinkingLevel,
    OpenAIResponsesCompat, ProviderEnv, ProviderHeaders, ProviderResponse, SessionAffinityFormat,
    SimpleStreamOptions, StopReason, StreamOptions, SuccessfulStopReason, ThinkingLevel, Tool,
    ToolChoice, Usage,
};
use crate::utils::error_body::{
    ProviderErrorData, format_provider_error, normalize_provider_error,
};
use crate::utils::pi_user_agent::get_pi_user_agent;
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::provider_retry::{
    ProviderErrorMetadata, ProviderRetryClassify, ProviderRetryError, ProviderRetryOptions,
    retry_provider_request,
};
use futures::{StreamExt, stream::BoxStream};
use openai_oxide::config::Config;
use openai_oxide::{OpenAI, OpenAIError, RequestOptions};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;
const OPENAI_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAIResponsesOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ThinkingLevel>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub reasoning_summary: Option<Option<ResponseReasoningSummary>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub service_tier: Option<Option<ResponseServiceTier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ResponseToolChoice>,
}

impl From<StreamOptions> for OpenAIResponsesOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAIResponsesApi;

pub fn open_ai_responses_api() -> OpenAIResponsesApi {
    OpenAIResponsesApi
}

impl ProviderStreams for OpenAIResponsesApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        match options {
            ApiStreamOptions::Base(options) => {
                stream(model, context, OpenAIResponsesOptions::from(options))
            }
            ApiStreamOptions::OpenAIResponses(options) => stream(model, context, options),
            ApiStreamOptions::OpenAICompletions(_)
            | ApiStreamOptions::OpenAICodexResponses(_)
            | ApiStreamOptions::Custom { .. } => {
                terminal_setup_error(model, "API options variant does not match openai-responses")
            }
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

#[derive(Debug, Clone)]
struct ResolvedCompat {
    session_affinity_format: SessionAffinityFormat,
    supports_long_cache_retention: bool,
    supports_strict_mode: bool,
    supports_open_ai_grammar_tools: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
    supports_explicit_prompt_cache_mode: bool,
}

#[derive(Debug, Clone)]
struct ResponsesClientConfig {
    api_key: String,
    base_url: String,
    authorization: Option<http::HeaderValue>,
    timeout_secs: u64,
}

impl Config for ResponsesClientConfig {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn api_key(&self) -> &str {
        &self.api_key
    }

    fn build_request(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(authorization) = &self.authorization {
            request.header(http::header::AUTHORIZATION, authorization.clone())
        } else {
            request.bearer_auth(&self.api_key)
        }
    }

    fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }

    fn max_retries(&self) -> u32 {
        0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum PromptCacheMode {
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct PromptCacheOptions {
    mode: PromptCacheMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ResponseReasoning {
    effort: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<ResponseReasoningSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct WireRequest {
    model: String,
    input: Vec<crate::api::openai_responses_shared::ResponseInputItem>,
    stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_cache_options: Option<PromptCacheOptions>,
    store: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_tier: Option<Option<ResponseServiceTier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponseTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ResponseToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponseReasoning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    include: Option<Vec<String>>,
}

pub fn stream(
    model: &Model,
    context: &Context,
    options: OpenAIResponsesOptions,
) -> AssistantMessageEventStream {
    let model = model.clone();
    let context = context.clone();
    let (sender, stream) = AssistantMessageEventStream::channel();
    tokio::spawn(async move {
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
    let mut lowered = OpenAIResponsesOptions {
        stream: build_base_options(
            model,
            context,
            Some(&options),
            options.stream.request.api_key.as_deref(),
        ),
        reasoning_effort: clamped.and_then(model_level_to_thinking),
        reasoning_summary: None,
        service_tier: None,
        tool_choice: options.tool_choice.map(|choice| {
            ResponseToolChoice::Mode(match choice {
                ToolChoice::Auto => ResponseToolChoiceMode::Auto,
                ToolChoice::None => ResponseToolChoiceMode::None,
            })
        }),
    };
    lowered.stream.request.api_key = options.stream.request.api_key;
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

fn pending_message(model: &Model) -> crate::types::AssistantMessage {
    crate::types::AssistantMessage::pending(
        model.api.clone(),
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

async fn run_stream(
    sender: AssistantStreamSender,
    model: Model,
    context: Context,
    options: OpenAIResponsesOptions,
) {
    let mut output = pending_message(&model);
    if let Err(error) = run_stream_inner(&sender, &model, &context, &options, &mut output).await {
        let aborted = options
            .stream
            .request
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
            || error.aborted;
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
    options: &OpenAIResponsesOptions,
    output: &mut crate::types::AssistantMessage,
) -> Result<(), ResponsesRunError> {
    if model.api.as_str() != "openai-responses" {
        return Err(ResponsesRunError::new(format!(
            "Model API {} does not match openai-responses",
            model.api
        )));
    }
    if model
        .compat
        .as_ref()
        .is_some_and(|compat| !matches!(compat, ModelCompat::OpenAIResponses(_)))
    {
        return Err(ResponsesRunError::new(
            "Model compat variant does not match openai-responses",
        ));
    }
    if options
        .stream
        .request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err(ResponsesRunError::aborted("Request was aborted"));
    }
    if options.stream.request.fetch.is_some() {
        return Err(ResponsesRunError::new(
            "Custom fetch is not supported by the openai-oxide transport",
        ));
    }
    let api_key = get_client_api_key(
        model.provider.as_str(),
        options.stream.request.api_key.as_deref(),
        options.stream.request.headers.as_ref(),
    )?;
    let cache_retention = resolve_cache_retention(
        options.stream.cache_retention,
        options.stream.request.env.as_ref(),
    );
    let cache_session_id = (cache_retention != CacheRetention::None)
        .then_some(options.stream.session_id.as_deref())
        .flatten();
    let compat = get_compat(model);
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat.supports_open_ai_grammar_tools,
    )
    .map_err(ResponsesRunError::display)?;
    let client = create_client(
        model,
        context,
        &api_key,
        options.stream.request.headers.as_ref(),
        cache_session_id,
        &compat,
        options.stream.request.timeout_ms,
    )?;
    let mut params = build_params(
        model,
        context,
        options,
        &compat,
        &grammar_tool_input_properties,
    )?;
    if let Some(on_payload) = &options.stream.request.on_payload
        && let Some(replacement) = on_payload(params.clone(), model).await
    {
        params = replacement;
    }

    let retry_options = ProviderRetryOptions {
        max_retries: options.stream.request.max_retries,
        max_retry_delay_ms: options.stream.request.max_retry_delay_ms,
        signal: options.stream.request.signal.clone(),
    };
    let sdk_stream = retry_provider_request(|| acquire_stream(&client, &params), retry_options)
        .await
        .map_err(format_retry_error)?;
    let mut sdk_stream =
        abortable_response_stream(sdk_stream, options.stream.request.signal.clone());

    if let Some(on_response) = &options.stream.request.on_response {
        // openai-oxide 0.16 consumes response metadata while constructing SseStream.
        on_response(
            ProviderResponse {
                status: 200,
                headers: BTreeMap::new(),
            },
            model,
        )
        .await;
    }
    sender
        .send(AssistantMessageEvent::Start)
        .map_err(ResponsesRunError::display)?;

    let apply_pricing = |usage: &mut Usage, service_tier: Option<Option<ResponseServiceTier>>| {
        apply_service_tier_pricing(usage, service_tier.flatten(), model);
    };
    process_responses_stream(
        &mut sdk_stream,
        output,
        sender,
        model,
        OpenAIResponsesStreamOptions {
            service_tier: options.service_tier,
            grammar_tool_input_properties: Some(&grammar_tool_input_properties),
            resolve_service_tier: None,
            apply_service_tier_pricing: Some(&apply_pricing),
            capture_end_turn: false,
        },
    )
    .await
    .map_err(ResponsesRunError::from_shared)?;

    if options
        .stream
        .request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return Err(ResponsesRunError::aborted("Request was aborted"));
    }
    if output.stop_reason == StopReason::Pending {
        return Err(ResponsesRunError::new(
            "OpenAI Responses stream ended without a stop reason",
        ));
    }
    if matches!(output.stop_reason, StopReason::Aborted | StopReason::Error) {
        return Err(ResponsesRunError::new(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_owned()),
        ));
    }
    let reason = successful_stop_reason(output.stop_reason).ok_or_else(|| {
        ResponsesRunError::new("Provider returned an invalid successful stop reason")
    })?;
    sender
        .send(AssistantMessageEvent::Done {
            reason,
            message: output.clone(),
        })
        .map_err(ResponsesRunError::display)
}

#[derive(Debug)]
struct ResponsesRunError {
    message: String,
    aborted: bool,
}

impl ResponsesRunError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: false,
        }
    }

    fn aborted(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: true,
        }
    }

    fn display(error: impl fmt::Display) -> Self {
        Self::new(error.to_string())
    }

    fn from_shared(error: OpenAIResponsesError) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for ResponsesRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
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

fn model_level_to_thinking(level: ModelThinkingLevel) -> Option<ThinkingLevel> {
    match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal => Some(ThinkingLevel::Minimal),
        ModelThinkingLevel::Low => Some(ThinkingLevel::Low),
        ModelThinkingLevel::Medium => Some(ThinkingLevel::Medium),
        ModelThinkingLevel::High => Some(ThinkingLevel::High),
        ModelThinkingLevel::Xhigh => Some(ThinkingLevel::Xhigh),
        ModelThinkingLevel::Max => Some(ThinkingLevel::Max),
    }
}

fn successful_stop_reason(reason: StopReason) -> Option<SuccessfulStopReason> {
    match reason {
        StopReason::Stop => Some(SuccessfulStopReason::Stop),
        StopReason::Length => Some(SuccessfulStopReason::Length),
        StopReason::ToolUse => Some(SuccessfulStopReason::ToolUse),
        StopReason::Deferred => Some(SuccessfulStopReason::Deferred),
        StopReason::Pending | StopReason::Error | StopReason::Aborted => None,
    }
}

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    headers.is_some_and(|headers| {
        headers.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case(name)
                && value.as_ref().is_some_and(|value| !value.trim().is_empty())
        })
    })
}

fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> Result<String, ResponsesRunError> {
    if let Some(api_key) = api_key.filter(|api_key| !api_key.is_empty()) {
        return Ok(api_key.to_owned());
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_owned());
    }
    Err(ResponsesRunError::new(format!(
        "No API key for provider: {provider}"
    )))
}

fn detect_session_affinity_format(model: &Model) -> SessionAffinityFormat {
    if model.provider.as_str() == "openrouter" || model.base_url.contains("openrouter.ai") {
        SessionAffinityFormat::Openrouter
    } else {
        SessionAffinityFormat::Openai
    }
}

fn resolve_cache_retention(
    cache_retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> CacheRetention {
    cache_retention.unwrap_or_else(|| {
        if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    })
}

fn get_compat(model: &Model) -> ResolvedCompat {
    let configured = match model.compat.as_ref() {
        Some(ModelCompat::OpenAIResponses(compat)) => compat,
        _ => &OpenAIResponsesCompat::default(),
    };
    ResolvedCompat {
        session_affinity_format: configured
            .session_affinity_format
            .unwrap_or_else(|| detect_session_affinity_format(model)),
        supports_long_cache_retention: configured.supports_long_cache_retention.unwrap_or(true),
        supports_strict_mode: configured.supports_strict_mode.unwrap_or(false),
        supports_open_ai_grammar_tools: configured.supports_open_ai_grammar_tools.unwrap_or(false),
        supports_additional_tools: configured.supports_additional_tools.unwrap_or(false),
        supports_tool_search: configured.supports_tool_search.unwrap_or(false),
        supports_explicit_prompt_cache_mode: configured
            .supports_explicit_prompt_cache_mode
            .unwrap_or(false),
    }
}

fn create_client(
    model: &Model,
    context: &Context,
    api_key: &str,
    options_headers: Option<&ProviderHeaders>,
    session_id: Option<&str>,
    compat: &ResolvedCompat,
    timeout_ms: Option<u64>,
) -> Result<OpenAI, ResponsesRunError> {
    let mut headers = BTreeMap::from([("User-Agent".to_owned(), get_pi_user_agent())]);
    if let Some(model_headers) = &model.headers {
        headers.extend(model_headers.clone());
    }
    if model.provider.as_str() == "github-copilot" {
        headers.extend(build_copilot_dynamic_headers(
            &context.messages,
            has_copilot_vision_input(&context.messages),
        ));
    }
    if let Some(session_id) = session_id {
        match compat.session_affinity_format {
            SessionAffinityFormat::Openrouter => {
                headers.insert("x-session-id".to_owned(), session_id.to_owned());
            }
            SessionAffinityFormat::Openai | SessionAffinityFormat::OpenaiNosession => {
                if compat.session_affinity_format == SessionAffinityFormat::Openai {
                    headers.insert("session_id".to_owned(), session_id.to_owned());
                }
                headers.insert("x-client-request-id".to_owned(), session_id.to_owned());
            }
        }
    }
    if let Some(options_headers) = options_headers {
        for (name, value) in options_headers {
            if let Some(value) = value {
                headers.retain(|key, _| !key.eq_ignore_ascii_case(name));
                headers.insert(name.clone(), value.clone());
            } else {
                headers.retain(|key, _| !key.eq_ignore_ascii_case(name));
            }
        }
    }

    let mut header_map = http::HeaderMap::new();
    for (name, value) in headers {
        let name = name
            .parse::<http::HeaderName>()
            .map_err(ResponsesRunError::display)?;
        let value = value
            .parse::<http::HeaderValue>()
            .map_err(ResponsesRunError::display)?;
        header_map.insert(name, value);
    }
    let authorization = header_map.remove(http::header::AUTHORIZATION);
    let config = ResponsesClientConfig {
        api_key: api_key.to_owned(),
        base_url: model.base_url.clone(),
        authorization,
        timeout_secs: timeout_ms.map_or(600, |timeout_ms| timeout_ms.div_ceil(1_000)),
    };
    let request_options = if let Some(timeout_ms) = timeout_ms {
        RequestOptions::new()
            .headers(header_map)
            .timeout(Duration::from_millis(timeout_ms))
    } else {
        RequestOptions::new().headers(header_map)
    };
    Ok(OpenAI::with_config(config).with_options(request_options))
}

fn prompt_cache_retention(
    compat: &ResolvedCompat,
    cache_retention: CacheRetention,
) -> Option<String> {
    (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention)
        .then(|| "24h".to_owned())
}

fn split_deferred_tools(context: &Context, enabled: bool) -> (Vec<Tool>, Vec<(String, Tool)>) {
    let mut unique = Vec::<(String, Tool)>::new();
    for tool in context.tools.iter().flatten() {
        if let Some((_, existing)) = unique.iter_mut().find(|(name, _)| name == &tool.name) {
            *existing = tool.clone();
        } else {
            unique.push((tool.name.clone(), tool.clone()));
        }
    }
    if !enabled {
        return (
            unique.into_iter().map(|(_, tool)| tool).collect(),
            Vec::new(),
        );
    }

    let mut deferred_names = BTreeSet::new();
    let mut used_names = BTreeSet::new();
    for message in &context.messages {
        match message {
            crate::types::Message::Assistant(message) => {
                for block in &message.content {
                    if let crate::types::AssistantContent::ToolCall(tool_call) = block {
                        used_names.insert(tool_call.name.clone());
                    }
                }
            }
            crate::types::Message::ToolResult(message) => {
                for name in message.added_tool_names.iter().flatten() {
                    if !used_names.contains(name) {
                        deferred_names.insert(name.clone());
                    }
                }
            }
            crate::types::Message::User(_) => {}
        }
    }
    let mut immediate = Vec::new();
    let mut deferred = Vec::new();
    for (name, tool) in unique {
        if deferred_names.contains(&name) {
            deferred.push((name, tool));
        } else {
            immediate.push(tool);
        }
    }
    (immediate, deferred)
}

fn thinking_mapping(model: &Model, level: ThinkingLevel) -> Option<&Option<String>> {
    let map = model.thinking_level_map.as_ref()?;
    match level {
        ThinkingLevel::Minimal => map.minimal.as_ref(),
        ThinkingLevel::Low => map.low.as_ref(),
        ThinkingLevel::Medium => map.medium.as_ref(),
        ThinkingLevel::High => map.high.as_ref(),
        ThinkingLevel::Xhigh => map.xhigh.as_ref(),
        ThinkingLevel::Max => map.max.as_ref(),
    }
}

fn thinking_level_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn build_params(
    model: &Model,
    context: &Context,
    options: &OpenAIResponsesOptions,
    compat: &ResolvedCompat,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Result<Value, ResponsesRunError> {
    let deferred_mode = if compat.supports_additional_tools {
        Some(DeferredResponsesToolsMode::AdditionalTools)
    } else if compat.supports_tool_search {
        Some(DeferredResponsesToolsMode::ToolSearch)
    } else {
        None
    };
    let (immediate_tools, deferred_tools) = split_deferred_tools(context, deferred_mode.is_some());
    let allowed_providers = OPENAI_TOOL_CALL_PROVIDERS
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let tool_options = ConvertResponsesToolsOptions {
        strict: None,
        supports_strict_mode: Some(compat.supports_strict_mode),
        supports_open_ai_grammar_tools: Some(compat.supports_open_ai_grammar_tools),
        defer_loading: false,
    };
    let input = convert_responses_messages(
        model,
        context,
        &allowed_providers,
        ConvertResponsesMessagesOptions {
            include_system_prompt: None,
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            deferred_tools: Some(&deferred_tools),
            deferred_tools_mode: deferred_mode,
            tool_options: tool_options.clone(),
        },
    )
    .map_err(ResponsesRunError::from_shared)?;
    let cache_retention = resolve_cache_retention(
        options.stream.cache_retention,
        options.stream.request.env.as_ref(),
    );
    let disable_implicit_prompt_cache =
        cache_retention == CacheRetention::None && compat.supports_explicit_prompt_cache_mode;
    let tools = (!immediate_tools.is_empty())
        .then(|| convert_responses_tools(immediate_tools.iter(), &tool_options))
        .transpose()
        .map_err(ResponsesRunError::from_shared)?;
    let mut reasoning = None;
    let mut include = None;
    if model.reasoning {
        let requested_summary = options.reasoning_summary.flatten();
        if options.reasoning_effort.is_some() || requested_summary.is_some() {
            let effort = options.reasoning_effort.map_or_else(
                || "medium".to_owned(),
                |effort| {
                    thinking_mapping(model, effort)
                        .and_then(|mapped| mapped.clone())
                        .unwrap_or_else(|| thinking_level_name(effort).to_owned())
                },
            );
            reasoning = Some(ResponseReasoning {
                effort,
                summary: Some(requested_summary.unwrap_or(ResponseReasoningSummary::Auto)),
            });
            include = Some(vec!["reasoning.encrypted_content".to_owned()]);
        } else if model.provider.as_str() != "github-copilot"
            && model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.off.as_ref())
                != Some(&None)
        {
            let effort = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.off.as_ref())
                .and_then(|mapped| mapped.clone())
                .unwrap_or_else(|| "none".to_owned());
            reasoning = Some(ResponseReasoning {
                effort,
                summary: None,
            });
        }
        if model.provider.as_str() == "xai" {
            include = Some(vec!["reasoning.encrypted_content".to_owned()]);
        }
    }

    let request = WireRequest {
        model: model.id.clone(),
        input,
        stream: true,
        prompt_cache_key: (cache_retention != CacheRetention::None)
            .then(|| clamp_open_ai_prompt_cache_key(options.stream.session_id.as_deref()))
            .flatten(),
        prompt_cache_retention: prompt_cache_retention(compat, cache_retention),
        prompt_cache_options: disable_implicit_prompt_cache.then_some(PromptCacheOptions {
            mode: PromptCacheMode::Explicit,
        }),
        store: false,
        max_output_tokens: options
            .stream
            .max_tokens
            .filter(|tokens| *tokens != 0)
            .map(|tokens| tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS)),
        temperature: options.stream.temperature,
        service_tier: options.service_tier,
        tools,
        tool_choice: options.tool_choice.clone(),
        reasoning,
        include,
    };
    let mut value = serde_json::to_value(request).map_err(ResponsesRunError::display)?;
    if let Some(sampling_params) = &options.stream.sampling_params
        && let Some(body) = value.as_object_mut()
    {
        body.extend(sampling_params.clone());
    }
    Ok(value)
}

fn service_tier_cost_multiplier(model: &Model, service_tier: Option<ResponseServiceTier>) -> f64 {
    match service_tier {
        Some(ResponseServiceTier::Flex) => 0.5,
        Some(ResponseServiceTier::Priority) if model.id == "gpt-5.5" => 2.5,
        Some(ResponseServiceTier::Priority) => 2.0,
        _ => 1.0,
    }
}

fn apply_service_tier_pricing(
    usage: &mut Usage,
    service_tier: Option<ResponseServiceTier>,
    model: &Model,
) {
    let multiplier = service_tier_cost_multiplier(model, service_tier);
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

#[derive(Debug)]
struct OxideRequestError {
    source: OpenAIError,
    metadata: Option<ProviderErrorMetadata>,
}

impl From<OpenAIError> for OxideRequestError {
    fn from(source: OpenAIError) -> Self {
        let metadata = match &source {
            OpenAIError::ApiError { status, .. } => Some(ProviderErrorMetadata {
                status: Some(*status),
                headers: BTreeMap::new(),
            }),
            OpenAIError::RequestError(_) => Some(ProviderErrorMetadata::default()),
            OpenAIError::JsonError(_)
            | OpenAIError::StreamError(_)
            | OpenAIError::InvalidArgument(_) => None,
        };
        Self { source, metadata }
    }
}

impl fmt::Display for OxideRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl ProviderRetryClassify for OxideRequestError {
    fn provider_error_metadata(&self) -> Option<&ProviderErrorMetadata> {
        self.metadata.as_ref()
    }

    fn provider_error_message(&self) -> String {
        self.to_string()
    }
}

async fn acquire_stream(
    client: &OpenAI,
    params: &Value,
) -> Result<BoxStream<'static, Result<Value, OpenAIError>>, OxideRequestError> {
    client
        .responses()
        .create_stream_raw(params)
        .await
        .map(|stream| stream.boxed())
        .map_err(Into::into)
}

fn format_oxide_error(error: OpenAIError) -> ResponsesRunError {
    let status = match &error {
        OpenAIError::ApiError { status, .. } => Some(i64::from(*status)),
        _ => None,
    };
    let data = ProviderErrorData {
        message: error.to_string(),
        status_code: status,
        ..ProviderErrorData::default()
    };
    ResponsesRunError::new(format_provider_error(
        &normalize_provider_error(&data),
        Some("OpenAI API error"),
    ))
}

fn format_retry_error(error: ProviderRetryError<OxideRequestError>) -> ResponsesRunError {
    match error {
        ProviderRetryError::Original(error) => format_oxide_error(error.source),
        ProviderRetryError::Abort => ResponsesRunError::aborted("Request aborted"),
        error @ ProviderRetryError::ServerDelay { .. } => ResponsesRunError::display(error),
    }
}

fn abortable_response_stream(
    stream: BoxStream<'static, Result<Value, OpenAIError>>,
    signal: Option<Arc<dyn AbortSignal>>,
) -> BoxStream<'static, Result<Value, OpenAIResponsesError>> {
    futures::stream::unfold((stream, signal, false), |(mut stream, signal, done)| async move {
        if done {
            return None;
        }
        let item = if let Some(signal) = signal.as_ref() {
            tokio::select! {
                item = stream.next() => item.map(|item| item.map_err(|error| OpenAIResponsesError::new(format_oxide_error(error).message))),
                () = signal.cancelled() => Some(Err(OpenAIResponsesError::new("Request was aborted"))),
            }
        } else {
            stream
                .next()
                .await
                .map(|item| item.map_err(|error| OpenAIResponsesError::new(format_oxide_error(error).message)))
        };
        item.map(|item| {
            let done = item.is_err();
            (item, (stream, signal, done))
        })
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ConstrainedSamplingConfig, ImageContent, Message, ModelCost, ModelCostRates,
        StrictPreference, TextContent, ThinkingLevelMap, ToolConstrainedSampling,
        ToolResultMessage, ToolResultRole, UserContent, UserContentBlock, UserMessage, UserRole,
    };
    use bytes::Bytes;
    use futures::StreamExt;
    use http_body_util::{BodyExt, Full};
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Mutex, PoisonError};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    #[derive(Clone)]
    struct Script {
        status: StatusCode,
        headers: BTreeMap<String, String>,
        body: String,
    }

    impl Script {
        fn sse(events: Vec<Value>) -> Self {
            let mut body = events
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            body.push_str("data: [DONE]\n\n");
            Self {
                status: StatusCode::OK,
                headers: BTreeMap::new(),
                body,
            }
        }

        fn error(status: StatusCode, message: &str) -> Self {
            Self {
                status,
                headers: BTreeMap::new(),
                body: json!({"error":{"message":message}}).to_string(),
            }
        }
    }

    #[derive(Debug, Clone)]
    struct CapturedRequest {
        path: String,
        headers: BTreeMap<String, String>,
        body: Value,
    }

    struct TestServer {
        base_url: String,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        task: JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn start_server(scripts: Vec<Script>) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let scripts = Arc::new(Mutex::new(VecDeque::from(scripts)));
        let task_captured = captured.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let captured = task_captured.clone();
                let scripts = scripts.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let captured = captured.clone();
                        let scripts = scripts.clone();
                        async move {
                            let path = request.uri().path().to_owned();
                            let headers = request
                                .headers()
                                .iter()
                                .filter_map(|(name, value)| {
                                    value
                                        .to_str()
                                        .ok()
                                        .map(|value| (name.as_str().to_owned(), value.to_owned()))
                                })
                                .collect();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            let body = serde_json::from_slice(&bytes).unwrap();
                            captured
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(CapturedRequest {
                                    path,
                                    headers,
                                    body,
                                });
                            let script = scripts
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .pop_front()
                                .unwrap_or_else(completed_script);
                            let mut response = Response::builder().status(script.status).header(
                                "content-type",
                                if script.status.is_success() {
                                    "text/event-stream"
                                } else {
                                    "application/json"
                                },
                            );
                            for (name, value) in script.headers {
                                response = response.header(name, value);
                            }
                            Ok::<_, std::convert::Infallible>(
                                response.body(Full::new(Bytes::from(script.body))).unwrap(),
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        TestServer {
            base_url: format!("http://{address}"),
            captured,
            task,
        }
    }

    fn completed_script() -> Script {
        Script::sse(vec![json!({
            "type":"response.completed",
            "response":{"id":"resp_test","status":"completed"}
        })])
    }

    fn base_model(base_url: String, id: &str) -> Model {
        Model {
            id: id.to_owned(),
            name: id.to_owned(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            base_url,
            reasoning: true,
            thinking_level_map: Some(ThinkingLevelMap {
                off: Some(Some("none".to_owned())),
                low: Some(Some("low".to_owned())),
                medium: Some(Some("medium".to_owned())),
                high: Some(Some("high".to_owned())),
                xhigh: Some(Some("xhigh".to_owned())),
                ..ThinkingLevelMap::default()
            }),
            input: vec![
                crate::types::ModelInput::Text,
                crate::types::ModelInput::Image,
            ],
            cost: ModelCost {
                rates: ModelCostRates {
                    input: if id == "gpt-5.5" { 5.0 } else { 2.5 },
                    output: if id == "gpt-5.5" { 30.0 } else { 15.0 },
                    cache_read: if id == "gpt-5.5" { 0.5 } else { 0.25 },
                    cache_write: 0.0,
                },
                tiers: None,
            },
            context_window: 272_000,
            max_tokens: 128_000,
            sampling_params: None,
            headers: None,
            compat: Some(ModelCompat::OpenAIResponses(OpenAIResponsesCompat {
                supports_strict_mode: Some(true),
                supports_open_ai_grammar_tools: Some(true),
                supports_additional_tools: Some(true),
                supports_tool_search: Some(true),
                ..OpenAIResponsesCompat::default()
            })),
        }
    }

    fn context() -> Context {
        Context {
            system_prompt: Some("sys".to_owned()),
            messages: vec![crate::types::Message::User(Box::new(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".to_owned()),
                timestamp: 1,
            }))],
            tools: None,
        }
    }

    fn options() -> OpenAIResponsesOptions {
        let mut options = OpenAIResponsesOptions::default();
        options.stream.request.api_key = Some("test-key".to_owned());
        options
    }

    fn payload(model: &Model, context: &Context, options: &OpenAIResponsesOptions) -> Value {
        let compat = get_compat(model);
        let grammar = create_grammar_tool_input_properties(
            context.tools.as_deref(),
            compat.supports_open_ai_grammar_tools,
        )
        .unwrap();
        build_params(model, context, options, &compat, &grammar).unwrap()
    }

    async fn run_and_capture(
        model: Model,
        context: Context,
        options: OpenAIResponsesOptions,
        server: &TestServer,
    ) -> (crate::types::AssistantMessage, CapturedRequest) {
        let mut events = stream(&model, &context, options);
        while events.next().await.is_some() {}
        let message = events.result().await.unwrap();
        let captured = server
            .captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last()
            .cloned()
            .expect("captured request");
        (message, captured)
    }

    fn response_compat(value: OpenAIResponsesCompat) -> Option<ModelCompat> {
        Some(ModelCompat::OpenAIResponses(value))
    }

    fn function_tool(name: &str) -> Tool {
        Tool {
            name: name.to_owned(),
            description: name.to_owned(),
            parameters: json!({"type":"object","properties":{"value":{"type":"string"}}}),
            constrained_sampling: None,
        }
    }

    /// Mirrors the request-relevant fields of the named generated OpenAI and
    /// GitHub Copilot catalog entries used by openai-responses-compat.test.ts.
    #[test]
    fn ports_compat_reasoning_and_tool_choice_matrix() {
        let mut github = base_model("https://example.invalid/v1".to_owned(), "gpt-5-mini");
        github.provider = "github-copilot".into();
        github.thinking_level_map.as_mut().unwrap().off = Some(None);
        github.compat = response_compat(OpenAIResponsesCompat {
            supports_open_ai_grammar_tools: Some(true),
            ..OpenAIResponsesCompat::default()
        });
        assert!(
            payload(&github, &context(), &options())
                .get("reasoning")
                .is_none()
        );

        let mut required_options = options();
        required_options.tool_choice =
            Some(ResponseToolChoice::Mode(ResponseToolChoiceMode::Required));
        let mut tool_context = context();
        tool_context.tools = Some(vec![function_tool("ping")]);
        let required = payload(
            &base_model("unused".to_owned(), "gpt-5.4"),
            &tool_context,
            &required_options,
        );
        assert_eq!(required["tool_choice"], "required");
        assert_eq!(required["tools"][0]["name"], "ping");
        assert_eq!(required["stream"], true);
        assert_eq!(required["store"], false);

        let off_supported = [
            "gpt-5.1",
            "gpt-5.2",
            "gpt-5.3-codex",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "gpt-5.5",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ];
        for id in off_supported {
            let model = base_model("unused".to_owned(), id);
            assert_eq!(
                payload(&model, &context(), &options())["reasoning"]["effort"],
                "none",
                "{id}"
            );
        }

        for id in [
            "gpt-5",
            "gpt-5-mini",
            "gpt-5-nano",
            "gpt-5-pro",
            "gpt-5.2-pro",
            "gpt-5.4-pro",
            "gpt-5.5-pro",
        ] {
            let mut model = base_model("unused".to_owned(), id);
            model.thinking_level_map.as_mut().unwrap().off = Some(None);
            assert!(
                payload(&model, &context(), &options())
                    .get("reasoning")
                    .is_none(),
                "{id}"
            );
        }
    }

    #[test]
    fn responses_options_preserve_explicit_nulls() {
        let options: OpenAIResponsesOptions = serde_json::from_value(json!({
            "reasoningSummary":null,
            "serviceTier":null
        }))
        .unwrap();
        assert_eq!(options.reasoning_summary, Some(None));
        assert_eq!(options.service_tier, Some(None));
        let value = serde_json::to_value(options).unwrap();
        assert_eq!(value["reasoningSummary"], Value::Null);
        assert_eq!(value["serviceTier"], Value::Null);
    }

    #[test]
    fn ports_cloudflare_explicit_strict_tools() {
        let mut cloudflare = base_model("unused".to_owned(), "gpt-5.6-sol");
        cloudflare.provider = "cloudflare-ai-gateway".into();
        cloudflare.thinking_level_map.as_mut().unwrap().off = Some(None);
        cloudflare.compat = response_compat(OpenAIResponsesCompat {
            supports_strict_mode: Some(true),
            supports_open_ai_grammar_tools: Some(true),
            ..OpenAIResponsesCompat::default()
        });
        let mut constrained = function_tool("constrained");
        constrained.constrained_sampling = Some(ToolConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema {
                strict: StrictPreference::Prefer,
            },
        ));
        let mut test_context = context();
        test_context.tools = Some(vec![function_tool("ordinary"), constrained]);
        let body = payload(&cloudflare, &test_context, &options());
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["tools"][1]["strict"], true);
    }

    /// Pins openai-responses.ts:272-351 field presence and custom-key precedence.
    #[test]
    fn request_fields_presence_reasoning_and_sampling_precedence_match_pi() {
        let mut model = base_model("unused".to_owned(), "gpt-5.4");
        let mut request_options = options();
        request_options.stream.max_tokens = Some(1);
        request_options.stream.temperature = Some(0.25);
        request_options.stream.session_id = Some("x".repeat(67));
        request_options.service_tier = Some(None);
        let body = payload(&model, &context(), &request_options);
        assert_eq!(body["max_output_tokens"], 16);
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["prompt_cache_key"], "x".repeat(64));
        assert_eq!(body["service_tier"], Value::Null);

        request_options.reasoning_effort = Some(ThinkingLevel::High);
        request_options.reasoning_summary = Some(None);
        let body = payload(&model, &context(), &request_options);
        assert_eq!(body["reasoning"], json!({"effort":"high","summary":"auto"}));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));

        request_options.reasoning_effort = None;
        request_options.reasoning_summary = Some(Some(ResponseReasoningSummary::Concise));
        let body = payload(&model, &context(), &request_options);
        assert_eq!(
            body["reasoning"],
            json!({"effort":"medium","summary":"concise"})
        );

        request_options.reasoning_summary = None;
        request_options.stream.sampling_params = Some(serde_json::Map::from_iter([
            ("model".to_owned(), json!("caller-model")),
            ("stream".to_owned(), json!(false)),
            ("store".to_owned(), json!(true)),
            ("max_output_tokens".to_owned(), json!(99)),
        ]));
        let body = payload(&model, &context(), &request_options);
        assert_eq!(body["model"], "caller-model");
        assert_eq!(body["stream"], false);
        assert_eq!(body["store"], true);
        assert_eq!(body["max_output_tokens"], 99);

        model.provider = "xai".into();
        model.thinking_level_map.as_mut().unwrap().off = Some(None);
        request_options.stream.sampling_params = None;
        let body = payload(&model, &context(), &request_options);
        assert!(body.get("reasoning").is_none());
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn prompt_cache_retention_presence_matches_pi() {
        let mut model = base_model("unused".to_owned(), "gpt-5.6-sol");
        model.compat = response_compat(OpenAIResponsesCompat {
            supports_long_cache_retention: Some(true),
            supports_explicit_prompt_cache_mode: Some(true),
            ..OpenAIResponsesCompat::default()
        });
        let mut request_options = options();
        request_options.stream.session_id = Some("session".to_owned());
        request_options.stream.cache_retention = Some(CacheRetention::Long);
        let body = payload(&model, &context(), &request_options);
        assert_eq!(body["prompt_cache_key"], "session");
        assert_eq!(body["prompt_cache_retention"], "24h");
        assert!(body.get("prompt_cache_options").is_none());

        request_options.stream.cache_retention = Some(CacheRetention::None);
        let body = payload(&model, &context(), &request_options);
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
        assert_eq!(body["prompt_cache_options"], json!({"mode":"explicit"}));
    }

    async fn affinity_request(
        server: &TestServer,
        provider: &str,
        format: Option<SessionAffinityFormat>,
        request_headers: Option<ProviderHeaders>,
        cache_retention: CacheRetention,
    ) -> CapturedRequest {
        let mut model = base_model(server.base_url.clone(), "gpt-5.4");
        model.provider = provider.into();
        model.compat = format.map(|session_affinity_format| {
            ModelCompat::OpenAIResponses(OpenAIResponsesCompat {
                session_affinity_format: Some(session_affinity_format),
                ..OpenAIResponsesCompat::default()
            })
        });
        let mut request_options = options();
        request_options.stream.session_id = Some("session-123".to_owned());
        request_options.stream.cache_retention = Some(cache_retention);
        request_options.stream.request.headers = request_headers;
        run_and_capture(model, context(), request_options, server)
            .await
            .1
    }

    fn header<'a>(request: &'a CapturedRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[tokio::test]
    async fn ports_cache_affinity_header_matrix_over_http() {
        let server = start_server(vec![completed_script(); 8]).await;

        let official = affinity_request(&server, "openai", None, None, CacheRetention::Short).await;
        assert_eq!(official.path, "/responses");
        assert_eq!(header(&official, "session_id"), Some("session-123"));
        assert_eq!(
            header(&official, "x-client-request-id"),
            Some("session-123")
        );

        let proxy = affinity_request(&server, "opencode", None, None, CacheRetention::Short).await;
        assert_eq!(header(&proxy, "session_id"), Some("session-123"));
        assert_eq!(header(&proxy, "x-client-request-id"), Some("session-123"));

        for (provider, configured) in [
            ("proxy", Some(SessionAffinityFormat::Openrouter)),
            ("openrouter", None),
        ] {
            let request =
                affinity_request(&server, provider, configured, None, CacheRetention::Short).await;
            assert_eq!(header(&request, "session_id"), None);
            assert_eq!(header(&request, "x-client-request-id"), None);
            assert_eq!(header(&request, "x-session-id"), Some("session-123"));
            assert_eq!(request.body["prompt_cache_key"], "session-123");
            assert!(request.body.get("session_id").is_none());
        }

        for provider in ["proxy", "opencode"] {
            let request = affinity_request(
                &server,
                provider,
                Some(SessionAffinityFormat::OpenaiNosession),
                None,
                CacheRetention::Short,
            )
            .await;
            assert_eq!(header(&request, "session_id"), None);
            assert_eq!(header(&request, "x-client-request-id"), Some("session-123"));
            assert_eq!(header(&request, "x-session-id"), None);
            assert_eq!(request.body["prompt_cache_key"], "session-123");
        }

        let override_headers = ProviderHeaders::from([
            ("session_id".to_owned(), Some("override-session".to_owned())),
            (
                "x-client-request-id".to_owned(),
                Some("override-request".to_owned()),
            ),
        ]);
        let overridden = affinity_request(
            &server,
            "openai",
            None,
            Some(override_headers),
            CacheRetention::Short,
        )
        .await;
        assert_eq!(header(&overridden, "session_id"), Some("override-session"));
        assert_eq!(
            header(&overridden, "x-client-request-id"),
            Some("override-request")
        );

        let disabled = affinity_request(&server, "openai", None, None, CacheRetention::None).await;
        assert_eq!(header(&disabled, "session_id"), None);
        assert_eq!(header(&disabled, "x-client-request-id"), None);
    }

    #[tokio::test]
    async fn ports_service_tier_cost_multipliers_over_http() {
        let scripts = [
            ("gpt-5.4", ResponseServiceTier::Priority, 2.0),
            ("gpt-5.5", ResponseServiceTier::Priority, 2.5),
            ("gpt-5.5", ResponseServiceTier::Flex, 0.5),
        ];
        let server = start_server(
            scripts
                .iter()
                .map(|(_, tier, _)| {
                    Script::sse(vec![json!({
                        "type":"response.completed",
                        "response":{
                            "status":"completed","service_tier":tier,
                            "usage":{
                                "input_tokens":100_000,"output_tokens":100_000,
                                "total_tokens":200_000,"input_tokens_details":{"cached_tokens":0}
                            }
                        }
                    })])
                })
                .collect(),
        )
        .await;
        for (id, tier, multiplier) in scripts {
            let mut request_options = options();
            request_options.service_tier = Some(Some(tier));
            let model = base_model(server.base_url.clone(), id);
            let expected_input = model.cost.rates.input * multiplier * 0.1;
            let expected_output = model.cost.rates.output * multiplier * 0.1;
            let (message, _) = run_and_capture(model, context(), request_options, &server).await;
            assert!((message.usage.cost.input - expected_input).abs() < f64::EPSILON);
            assert!((message.usage.cost.output - expected_output).abs() < f64::EPSILON);
            assert!(
                (message.usage.cost.total - expected_input - expected_output).abs() < f64::EPSILON
            );
        }
    }

    #[tokio::test]
    async fn ports_tool_result_images_inside_function_output_over_http() {
        for provider in ["openai", "github-copilot"] {
            let server = start_server(vec![
                Script::sse(vec![
                    json!({
                        "type":"response.output_item.added","output_index":0,
                        "item":{
                            "type":"function_call","id":"fc_image","call_id":"call_image",
                            "name":"get_circle_with_description","arguments":""
                        }
                    }),
                    json!({
                        "type":"response.output_item.done","output_index":0,
                        "item":{
                            "type":"function_call","id":"fc_image","call_id":"call_image",
                            "name":"get_circle_with_description","arguments":"{}"
                        }
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":"resp_tool","status":"completed"}
                    }),
                ]),
                Script::sse(vec![
                    json!({
                        "type":"response.output_item.added","output_index":0,
                        "item":{"type":"message","id":"msg_answer","content":[]}
                    }),
                    json!({
                        "type":"response.output_item.done","output_index":0,
                        "item":{
                            "type":"message","id":"msg_answer",
                            "content":[{"type":"output_text","text":"The image is a red circle."}]
                        }
                    }),
                    json!({
                        "type":"response.completed",
                        "response":{"id":"resp_answer","status":"completed"}
                    }),
                ]),
            ])
            .await;
            let mut model = base_model(server.base_url.clone(), "gpt-5-mini");
            model.provider = provider.into();
            let mut test_context = context();
            test_context.tools = Some(vec![function_tool("get_circle_with_description")]);

            let (first, _) =
                run_and_capture(model.clone(), test_context.clone(), options(), &server).await;
            assert_eq!(first.stop_reason, StopReason::ToolUse);
            let call = first
                .content
                .iter()
                .find_map(|block| match block {
                    crate::types::AssistantContent::ToolCall(call) => Some(call),
                    _ => None,
                })
                .expect("tool call");
            test_context
                .messages
                .push(Message::Assistant(Box::new(first.clone())));
            test_context
                .messages
                .push(Message::ToolResult(Box::new(ToolResultMessage {
                    role: ToolResultRole::ToolResult,
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: vec![
                        UserContentBlock::Text(TextContent::new(
                            "A red circle with a diameter of 100 pixels.",
                        )),
                        UserContentBlock::Image(ImageContent::new("iVBORw0KGgo=", "image/png")),
                    ],
                    details: None,
                    usage: None,
                    added_tool_names: None,
                    is_error: false,
                    timestamp: 2,
                })));
            let (second, captured) = run_and_capture(model, test_context, options(), &server).await;
            assert_eq!(second.stop_reason, StopReason::Stop);
            assert!(second.content.iter().any(|block| {
                matches!(block, crate::types::AssistantContent::Text(text) if text.text.contains("red circle"))
            }));

            let input = captured.body["input"].as_array().expect("input array");
            let output_index = input
                .iter()
                .position(|item| item["type"] == "function_call_output")
                .expect("function-call output");
            let output = input[output_index]["output"]
                .as_array()
                .expect("content-array output");
            assert!(output.iter().any(|item| {
                item["type"] == "input_text"
                    && item["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("red circle"))
            }));
            assert!(output.iter().any(|item| {
                item["type"] == "input_image"
                    && item["image_url"]
                        .as_str()
                        .is_some_and(|url| url.starts_with("data:image/png;base64,"))
            }));
            assert!(
                !input[output_index + 1..]
                    .iter()
                    .any(|item| item["role"] == "user")
            );
        }
    }

    #[tokio::test]
    async fn ports_wrapper_early_eof_as_terminal_error() {
        let server = start_server(vec![Script::sse(vec![
            json!({"type":"response.created","response":{"id":"resp_wrapper_early_eof"}}),
            json!({
                "type":"response.output_item.added","output_index":0,
                "item":{"type":"reasoning","id":"rs_wrapper_early_eof","summary":[]}
            }),
            json!({
                "type":"response.reasoning_text.delta","output_index":0,
                "delta":"partial reasoning before the wrapper stream ends"
            }),
        ])])
        .await;
        let model = base_model(server.base_url.clone(), "gpt-5-mini");
        let mut events = stream(&model, &context(), options());
        let mut emitted = Vec::new();
        while let Some(event) = events.next().await {
            emitted.push(event);
        }
        let result = events.result().await.unwrap();
        assert!(matches!(
            emitted.first(),
            Some(AssistantMessageEvent::Start)
        ));
        assert!(matches!(
            emitted.last(),
            Some(AssistantMessageEvent::Error { .. })
        ));
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("OpenAI Responses stream ended before a terminal response event")
        );
    }

    #[tokio::test]
    async fn raw_transport_ignores_unknown_events_and_runs_hooks() {
        let server = start_server(vec![Script::sse(vec![
            json!({"type":"response.future_event","unknown":true}),
            json!({"type":"response.completed","response":{"id":"resp_test","status":"completed"}}),
        ])])
        .await;
        let observed_response = Arc::new(Mutex::new(None::<ProviderResponse>));
        let hook_response = observed_response.clone();
        let mut request_options = options();
        request_options.stream.request.on_payload = Some(Arc::new(|mut body, _| {
            Box::pin(async move {
                body.as_object_mut()
                    .expect("request object")
                    .insert("hook_field".to_owned(), json!("present"));
                Some(body)
            })
        }));
        request_options.stream.request.on_response = Some(Arc::new(move |response, _| {
            let observed = hook_response.clone();
            Box::pin(async move {
                *observed.lock().unwrap_or_else(PoisonError::into_inner) = Some(response);
            })
        }));
        let model = base_model(server.base_url.clone(), "gpt-5.4");
        let (message, captured) = run_and_capture(model, context(), request_options, &server).await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(captured.body["hook_field"], "present");
        assert_eq!(
            observed_response
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(|response| response.status),
            Some(200)
        );
    }

    #[tokio::test]
    async fn retry_provider_request_owns_initial_call_retries() {
        let server = start_server(vec![
            Script::error(StatusCode::INTERNAL_SERVER_ERROR, "retry me"),
            completed_script(),
        ])
        .await;
        let model = base_model(server.base_url.clone(), "gpt-5.4");
        let mut request_options = options();
        request_options.stream.request.max_retries = Some(1);
        let (message, _) = run_and_capture(model, context(), request_options, &server).await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(
            server
                .captured
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
            2
        );
    }
}
