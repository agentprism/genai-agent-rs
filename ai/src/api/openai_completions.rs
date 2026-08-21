//! OpenAI Chat Completions ⇐ pi `src/api/openai-completions.ts`.

use crate::api::constrained_sampling::{
    GrammarConstrainedSamplingFormat, GrammarToolInputJsonBuffer,
    append_grammar_tool_input_json_delta, create_grammar_tool_input_properties,
    get_grammar_tool_input, get_json_schema_tool_parameters, resolve_grammar_constrained_sampling,
    resolve_json_schema_strict_sampling,
};
use crate::api::github_copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use crate::api::openai_prompt_cache::clamp_open_ai_prompt_cache_key;
use crate::api::openai_sse::{OpenAiHttpError, OpenAiSseError, OpenAiSseRequest, acquire_sse};
use crate::api::simple_options::{
    build_base_options, clamp_thinking_budget_to_answer_room, thinking_budget_for_level,
};
use crate::api::transform_messages::{
    normalize_open_ai_completions_tool_call_id, transform_messages,
};
use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::event_stream::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantStreamSender,
};
use crate::models::{calculate_cost, clamp_thinking_level};
#[cfg(test)]
use crate::types::ModelCompat;
use crate::types::{
    AssistantContent, AssistantMessage, CacheControlFormat, CacheRetention, ChatTemplateKwargValue,
    Context, DeferredToolsMode, ErrorStopReason, ImageContent, MaxTokensField, Message, Model,
    ModelInput, ModelThinkingLevel, OpenAICompletionsCompat, OpenRouterRouting, ProviderEnv,
    ProviderHeaders, SessionAffinityFormat, SimpleStreamOptions, StopReason, StreamOptions,
    SuccessfulStopReason, TextContent, ThinkingBudgets, ThinkingContent, ThinkingFormat,
    ThinkingLevel, ThinkingTokenBudgetField, ThinkingVariable, Tool, ToolCall, ToolChoice,
    ToolResultMessage, Usage, UsageValue, VercelGatewayRouting, serialize_optional_js_f64,
};
use crate::utils::json_parse::parse_streaming_json;
use crate::utils::pi_user_agent::{get_pi_user_agent, openai_sdk_platform_headers};
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::provider_retry::{
    ProviderRetryError, ProviderRetryOptions, retry_provider_request,
};
use crate::utils::sanitize_unicode::sanitize_surrogates;
use futures::{FutureExt, StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenAIChatToolChoice {
    Mode(OpenAIChatToolChoiceMode),
    Function {
        #[serde(rename = "type")]
        kind: FunctionToolType,
        function: OpenAIChatToolChoiceFunction,
    },
    Custom {
        #[serde(rename = "type")]
        kind: CustomToolType,
        custom: OpenAIChatToolChoiceFunction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAIChatToolChoiceMode {
    None,
    Auto,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAIChatToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FunctionToolType {
    Function,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CustomToolType {
    Custom,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICompletionsOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<OpenAIChatToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
}

impl From<StreamOptions> for OpenAICompletionsOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAICompletionsApi;

pub fn open_ai_completions_api() -> OpenAICompletionsApi {
    OpenAICompletionsApi
}

impl ProviderStreams for OpenAICompletionsApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        match options {
            ApiStreamOptions::Base(options) => {
                stream(model, context, OpenAICompletionsOptions::from(options))
            }
            ApiStreamOptions::OpenAICompletions(options) => stream(model, context, options),
            ApiStreamOptions::AnthropicMessages(_)
            | ApiStreamOptions::OpenAIResponses(_)
            | ApiStreamOptions::OpenAICodexResponses(_)
            | ApiStreamOptions::Custom { .. } => terminal_setup_error(
                model,
                "API options variant does not match openai-completions",
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

#[derive(Debug, Clone)]
struct ResolvedCompat {
    supports_store: bool,
    supports_developer_role: bool,
    supports_reasoning_effort: bool,
    supports_usage_in_streaming: bool,
    supports_finish_reason: bool,
    max_tokens_field: MaxTokensField,
    requires_tool_result_name: bool,
    requires_assistant_after_tool_result: bool,
    requires_thinking_as_text: bool,
    requires_reasoning_content_on_assistant_messages: bool,
    thinking_format: ThinkingFormat,
    chat_template_kwargs: BTreeMap<String, ChatTemplateKwargValue>,
    chat_template_args: BTreeMap<String, ChatTemplateKwargValue>,
    open_router_routing: Option<OpenRouterRouting>,
    vercel_gateway_routing: Option<VercelGatewayRouting>,
    zai_tool_stream: bool,
    thinking_token_budget_field: Option<ThinkingTokenBudgetField>,
    supports_thinking_token_budget: Option<bool>,
    supports_open_ai_grammar_tools: bool,
    supports_strict_mode: bool,
    cache_control_format: Option<CacheControlFormat>,
    send_session_affinity_headers: bool,
    deferred_tools_mode: Option<DeferredToolsMode>,
    session_affinity_format: SessionAffinityFormat,
    supports_long_cache_retention: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompatCacheControl {
    #[serde(rename = "type")]
    kind: CacheControlType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CacheControlType {
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum WireContent {
    String(String),
    Parts(Vec<WireContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentPart {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CompatCacheControl>,
    },
    ImageUrl {
        image_url: WireImageUrl,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WireImageUrl {
    url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum WireMessage {
    System {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<WireContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<WireTool>>,
    },
    Developer {
        content: WireContent,
    },
    User {
        content: WireContent,
    },
    Assistant {
        #[serde(default)]
        content: Option<WireContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<WireAssistantToolCall>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_details: Option<Vec<Value>>,
    },
    Tool {
        content: WireContent,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WireAssistantToolCall {
    Function {
        id: String,
        function: WireFunctionCall,
    },
    Custom {
        id: String,
        custom: WireCustomCall,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WireCustomCall {
    name: String,
    input: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct WireTool {
    #[serde(flatten)]
    definition: WireToolDefinition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_control: Option<CompatCacheControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WireToolDefinition {
    Function { function: WireFunctionDefinition },
    Custom { custom: WireCustomDefinition },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct WireFunctionDefinition {
    name: String,
    description: String,
    parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WireCustomDefinition {
    name: String,
    description: String,
    format: WireCustomFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WireCustomFormat {
    #[serde(rename = "type")]
    kind: GrammarType,
    grammar: WireGrammar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GrammarType {
    Grammar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WireGrammar {
    syntax: GrammarSyntax,
    definition: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GrammarSyntax {
    Lark,
    Regex,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct WireRequest {
    model: String,
    messages: Vec<WireMessage>,
    stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream_options: Option<WireStreamOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_js_f64"
    )]
    temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<WireTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_choice: Option<OpenAIChatToolChoice>,
    #[serde(flatten)]
    dialect: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct WireStreamOptions {
    include_usage: bool,
}

pub fn stream(
    model: &Model,
    context: &Context,
    options: OpenAICompletionsOptions,
) -> AssistantMessageEventStream {
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
    let mut base = OpenAICompletionsOptions {
        stream: build_base_options(
            model,
            context,
            Some(&options),
            options.stream.request.api_key.as_deref(),
        ),
        tool_choice: options.tool_choice.map(|choice| match choice {
            ToolChoice::Auto => OpenAIChatToolChoice::Mode(OpenAIChatToolChoiceMode::Auto),
            ToolChoice::None => OpenAIChatToolChoice::Mode(OpenAIChatToolChoiceMode::None),
        }),
        reasoning_effort: None,
        thinking_budgets: options.thinking_budgets,
    };
    if let Some(reasoning) = options.reasoning {
        let clamped = clamp_thinking_level(model, thinking_to_model_level(reasoning));
        base.reasoning_effort = model_level_to_thinking(clamped);
    }
    stream(model, context, base)
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
    options: OpenAICompletionsOptions,
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
        Err(CompletionError::new(
            crate::utils::diagnostics::format_panic_payload(panic.as_ref()),
        ))
    });
    if let Err(error) = result {
        output.stop_reason = if options
            .stream
            .request
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
            || error.aborted
        {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        output.error_message = Some(error.message);
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
}

async fn run_stream_inner(
    sender: &AssistantStreamSender,
    model: &Model,
    context: &Context,
    options: &OpenAICompletionsOptions,
    output: &mut AssistantMessage,
) -> Result<(), CompletionError> {
    let api_key = get_client_api_key(
        model.provider.as_str(),
        options.stream.request.api_key.as_deref(),
        options.stream.request.headers.as_ref(),
    )?;
    let compat = get_compat(model);
    let grammar_tool_input_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat.supports_open_ai_grammar_tools,
    )
    .map_err(CompletionError::display)?;
    let cache_retention = resolve_cache_retention(
        options.stream.cache_retention,
        options.stream.request.env.as_ref(),
    );
    let cache_session_id = (cache_retention != CacheRetention::None)
        .then_some(options.stream.session_id.as_deref())
        .flatten();
    let headers = create_headers(
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
        cache_retention,
        &grammar_tool_input_properties,
    )?;
    if let Some(on_payload) = &options.stream.request.on_payload
        && let Some(replacement) = on_payload(params.clone(), model)
            .await
            .map_err(CompletionError::new)?
    {
        params = replacement;
    }

    let retry_options = ProviderRetryOptions {
        max_retries: options.stream.request.max_retries,
        max_retry_delay_ms: options.stream.request.max_retry_delay_ms,
        signal: options.stream.request.signal.clone(),
    };
    let request = OpenAiSseRequest {
        url: format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
        headers,
        body: serde_json::to_vec(&params).map_err(CompletionError::display)?,
        fetch: options.stream.request.fetch.clone(),
        signal: options.stream.request.signal.clone(),
        timeout_ms: options.stream.request.timeout_ms,
    };
    let acquired = retry_provider_request(|| acquire_sse(&request), retry_options)
        .await
        .map_err(format_retry_error)?;

    if let Some(on_response) = &options.stream.request.on_response {
        on_response(acquired.response.clone(), model)
            .await
            .map_err(CompletionError::new)?;
    }

    sender
        .send(AssistantMessageEvent::Start)
        .map_err(CompletionError::display)?;
    let mut sdk_stream = acquired.stream;
    let mut state = StreamingState::default();
    while let Some(chunk) = next_chunk(&mut sdk_stream, options).await? {
        process_chunk(
            sender,
            model,
            &compat,
            &grammar_tool_input_properties,
            output,
            &mut state,
            chunk,
        )?;
    }
    finish_blocks(sender, output, &mut state)?;

    if options
        .stream
        .request
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
        || output.stop_reason == StopReason::Aborted
    {
        return Err(CompletionError::aborted("Request was aborted"));
    }
    if !state.has_finish_reason && !compat.supports_finish_reason {
        output.stop_reason = if output
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
        {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
    }
    if output.stop_reason == StopReason::Error {
        return Err(CompletionError::new(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "Provider returned an error stop reason".to_owned()),
        ));
    }
    if (compat.supports_finish_reason && !state.has_finish_reason)
        || output.stop_reason == StopReason::Pending
    {
        return Err(CompletionError::new("Stream ended without finish_reason"));
    }
    let reason = successful_stop_reason(output.stop_reason).ok_or_else(|| {
        CompletionError::new("Provider returned an invalid successful stop reason")
    })?;
    sender
        .send(AssistantMessageEvent::Done {
            reason,
            message: output.clone(),
        })
        .map_err(CompletionError::display)
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

#[derive(Debug)]
struct CompletionError {
    message: String,
    aborted: bool,
}

impl CompletionError {
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
}

impl fmt::Display for CompletionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
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
) -> Result<String, CompletionError> {
    if let Some(api_key) = api_key.filter(|api_key| !api_key.is_empty()) {
        return Ok(api_key.to_owned());
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_owned());
    }
    Err(CompletionError::new(format!(
        "No API key for provider: {provider}"
    )))
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

fn create_headers(
    model: &Model,
    context: &Context,
    api_key: &str,
    options_headers: Option<&ProviderHeaders>,
    session_id: Option<&str>,
    compat: &ResolvedCompat,
    timeout_ms: Option<f64>,
) -> Result<BTreeMap<String, String>, CompletionError> {
    let mut headers = BTreeMap::from([
        ("Accept".to_owned(), "application/json".to_owned()),
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), get_pi_user_agent()),
    ]);
    headers.extend(openai_sdk_platform_headers(timeout_ms));
    if let Some(model_headers) = &model.headers {
        headers.extend(model_headers.clone());
    }
    if model.provider.as_str() == "github-copilot" {
        headers.extend(build_copilot_dynamic_headers(
            &context.messages,
            has_copilot_vision_input(&context.messages),
        ));
    }
    if let Some(session_id) = session_id.filter(|_| compat.send_session_affinity_headers) {
        match compat.session_affinity_format {
            SessionAffinityFormat::Openrouter => {
                headers.insert("x-session-id".to_owned(), session_id.to_owned());
            }
            SessionAffinityFormat::Openai | SessionAffinityFormat::OpenaiNosession => {
                if compat.session_affinity_format == SessionAffinityFormat::Openai {
                    headers.insert("session_id".to_owned(), session_id.to_owned());
                }
                headers.insert("x-client-request-id".to_owned(), session_id.to_owned());
                headers.insert("x-session-affinity".to_owned(), session_id.to_owned());
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
    if !headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
    {
        headers.insert("Authorization".to_owned(), format!("Bearer {api_key}"));
    }
    Ok(headers)
}

async fn next_chunk(
    stream: &mut BoxStream<'static, Result<Value, OpenAiSseError>>,
    options: &OpenAICompletionsOptions,
) -> Result<Option<RawChunk>, CompletionError> {
    let next = async {
        match stream.next().await {
            Some(Ok(chunk)) if chunk.is_object() => serde_json::from_value(chunk)
                .map(Some)
                .map_err(|error| OpenAiSseError::new(error.to_string())),
            Some(Ok(_)) => Ok(Some(RawChunk::default())),
            Some(Err(error)) => Err(error),
            None => Ok(None),
        }
    };
    let result = if let Some(signal) = &options.stream.request.signal {
        tokio::select! {
            biased;
            result = next => result,
            () = signal.cancelled() => return Ok(None),
        }
    } else {
        next.await
    };
    match result {
        Err(error) if error.aborted_flag() => Ok(None),
        Err(error) => Err(CompletionError::new(error.formatted(true))),
        Ok(chunk) => Ok(chunk),
    }
}

fn format_retry_error(error: ProviderRetryError<OpenAiHttpError>) -> CompletionError {
    match error {
        ProviderRetryError::Original(error) => {
            let aborted = error.aborted();
            let message = error.formatted(None, true);
            if aborted {
                CompletionError::aborted(message)
            } else {
                CompletionError::new(message)
            }
        }
        ProviderRetryError::Abort => CompletionError::aborted("Request aborted"),
        error @ ProviderRetryError::ServerDelay { .. } => CompletionError::display(error),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawChunk {
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    model: Option<String>,
    #[serde(default, deserialize_with = "deserialize_struct_or_none")]
    usage: Option<RawUsage>,
    #[serde(default, deserialize_with = "deserialize_vec_or_default")]
    choices: Vec<RawChoice>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawChoice {
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    finish_reason: Option<String>,
    #[serde(default, deserialize_with = "deserialize_struct_or_none")]
    usage: Option<RawUsage>,
    #[serde(default, deserialize_with = "deserialize_struct_or_none")]
    delta: Option<RawDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawDelta {
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    content: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    reasoning: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    reasoning_text: Option<String>,
    #[serde(default, deserialize_with = "deserialize_vec_or_default")]
    tool_calls: Vec<RawToolCallDelta>,
    #[serde(default, deserialize_with = "deserialize_vec_or_default")]
    reasoning_details: Vec<Value>,
}

fn deserialize_vec_or_default<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = Value::deserialize(deserializer)?;
    Ok(value.as_array().map_or_else(Vec::new, |values| {
        values
            .iter()
            .cloned()
            .map(|value| serde_json::from_value(value).unwrap_or_default())
            .collect()
    }))
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Value::deserialize(deserializer)?
        .as_str()
        .map(str::to_owned))
}

fn deserialize_struct_or_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Ok(None);
    }
    Ok(serde_json::from_value(value).ok())
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawToolCallDelta {
    #[serde(default, deserialize_with = "deserialize_optional_usize")]
    index: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_struct_or_none")]
    function: Option<RawFunctionDelta>,
    #[serde(default, deserialize_with = "deserialize_struct_or_none")]
    custom: Option<RawCustomDelta>,
}

fn deserialize_optional_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer)?
        .as_u64()
        .map(usize::try_from)
        .transpose()
        .map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawFunctionDelta {
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    arguments: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawCustomDelta {
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    input: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawUsage {
    #[serde(default)]
    prompt_tokens: Option<UsageValue>,
    #[serde(default)]
    completion_tokens: Option<UsageValue>,
    #[serde(default)]
    cached_tokens: Option<UsageValue>,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<UsageValue>,
    #[serde(default, deserialize_with = "deserialize_struct_or_none")]
    prompt_tokens_details: Option<RawPromptDetails>,
    #[serde(default, deserialize_with = "deserialize_struct_or_none")]
    completion_tokens_details: Option<RawCompletionDetails>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawPromptDetails {
    #[serde(default)]
    cached_tokens: Option<UsageValue>,
    #[serde(default)]
    cache_write_tokens: Option<UsageValue>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawCompletionDetails {
    #[serde(default)]
    reasoning_tokens: Option<UsageValue>,
}

#[derive(Debug, Default)]
struct StreamingState {
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    has_finish_reason: bool,
    tool_by_stream_index: BTreeMap<usize, usize>,
    tool_by_id: BTreeMap<String, usize>,
    tool_scratch: BTreeMap<usize, ToolScratch>,
    pending_reasoning_details_by_tool_call_id: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct ToolScratch {
    partial_args: String,
    custom_input: Option<CustomInputScratch>,
}

#[derive(Debug)]
struct CustomInputScratch {
    property: String,
    json_buffer: GrammarToolInputJsonBuffer,
}

fn process_chunk(
    sender: &AssistantStreamSender,
    model: &Model,
    _compat: &ResolvedCompat,
    grammar_tool_input_properties: &BTreeMap<String, String>,
    output: &mut AssistantMessage,
    state: &mut StreamingState,
    chunk: RawChunk,
) -> Result<(), CompletionError> {
    if output.response_id.as_deref().is_none_or(str::is_empty)
        && let Some(id) = chunk.id
    {
        output.response_id = Some(id);
    }
    if output.response_model.is_none()
        && let Some(response_model) = chunk
            .model
            .filter(|response_model| !response_model.is_empty() && response_model != &model.id)
    {
        output.response_model = Some(response_model);
    }
    if let Some(usage) = chunk.usage.as_ref() {
        output.usage = parse_chunk_usage(usage, model);
    }
    let Some(choice) = chunk.choices.into_iter().next() else {
        return Ok(());
    };
    if chunk.usage.is_none()
        && let Some(usage) = choice.usage.as_ref()
    {
        output.usage = parse_chunk_usage(usage, model);
    }
    if let Some(reason) = choice.finish_reason.filter(|reason| !reason.is_empty()) {
        output.raw_stop_reason = Some(reason.clone());
        let (stop_reason, error_message) = map_stop_reason(&reason);
        output.stop_reason = stop_reason;
        output.error_message = error_message;
        state.has_finish_reason = true;
    }
    let Some(delta) = choice.delta else {
        return Ok(());
    };
    if let Some(content) = delta.content.filter(|content| !content.is_empty()) {
        let index = ensure_text_block(sender, output, state)?;
        let Some(AssistantContent::Text(block)) = output.content.get_mut(index) else {
            return Err(CompletionError::new("text block state is invalid"));
        };
        block.text.push_str(&content);
        sender
            .send(AssistantMessageEvent::TextDelta {
                content_index: index,
                delta: content,
            })
            .map_err(CompletionError::display)?;
    }
    let reasoning = [
        ("reasoning_content", delta.reasoning_content),
        ("reasoning", delta.reasoning),
        ("reasoning_text", delta.reasoning_text),
    ]
    .into_iter()
    .find_map(|(field, value)| {
        value
            .filter(|value| !value.is_empty())
            .map(|value| (field, value))
    });
    if let Some((field, reasoning)) = reasoning {
        let signature = if model.provider.as_str() == "opencode-go" && field == "reasoning" {
            "reasoning_content"
        } else {
            field
        };
        let index = ensure_thinking_block(sender, output, state, signature)?;
        let Some(AssistantContent::Thinking(block)) = output.content.get_mut(index) else {
            return Err(CompletionError::new("thinking block state is invalid"));
        };
        block.thinking.push_str(&reasoning);
        sender
            .send(AssistantMessageEvent::ThinkingDelta {
                content_index: index,
                delta: reasoning,
                thinking_signature_delta: None,
            })
            .map_err(CompletionError::display)?;
    }
    for tool_delta in delta.tool_calls {
        process_tool_delta(
            sender,
            grammar_tool_input_properties,
            output,
            state,
            tool_delta,
        )?;
    }
    for detail in delta.reasoning_details {
        if !is_open_ai_reasoning_detail(&detail) {
            continue;
        }
        output
            .reasoning_details
            .get_or_insert_with(Vec::new)
            .push(detail.clone());
        if let Some((id, serialized)) = encrypted_reasoning_detail(&detail) {
            if let Some(index) = state.tool_by_id.get(id).copied() {
                if let Some(AssistantContent::ToolCall(call)) = output.content.get_mut(index) {
                    call.thought_signature = Some(serialized);
                }
            } else {
                state
                    .pending_reasoning_details_by_tool_call_id
                    .insert(id.to_owned(), serialized);
            }
        }
    }
    Ok(())
}

fn ensure_text_block(
    sender: &AssistantStreamSender,
    output: &mut AssistantMessage,
    state: &mut StreamingState,
) -> Result<usize, CompletionError> {
    if let Some(index) = state.text_index {
        return Ok(index);
    }
    let index = output.content.len();
    output
        .content
        .push(AssistantContent::Text(TextContent::new("")));
    state.text_index = Some(index);
    sender
        .send(AssistantMessageEvent::TextStart {
            content_index: index,
        })
        .map_err(CompletionError::display)?;
    Ok(index)
}

fn ensure_thinking_block(
    sender: &AssistantStreamSender,
    output: &mut AssistantMessage,
    state: &mut StreamingState,
    signature: &str,
) -> Result<usize, CompletionError> {
    if let Some(index) = state.thinking_index {
        return Ok(index);
    }
    let index = output.content.len();
    let mut block = ThinkingContent::new("");
    block.thinking_signature = Some(signature.to_owned());
    output.content.push(AssistantContent::Thinking(block));
    state.thinking_index = Some(index);
    sender
        .send(AssistantMessageEvent::ThinkingStart {
            content_index: index,
            thinking: None,
            thinking_signature: Some(signature.to_owned()),
            redacted: None,
        })
        .map_err(CompletionError::display)?;
    Ok(index)
}

fn process_tool_delta(
    sender: &AssistantStreamSender,
    grammar_tool_input_properties: &BTreeMap<String, String>,
    output: &mut AssistantMessage,
    state: &mut StreamingState,
    delta: RawToolCallDelta,
) -> Result<(), CompletionError> {
    let name = delta
        .function
        .as_ref()
        .and_then(|function| function.name.as_deref())
        .or_else(|| {
            delta
                .custom
                .as_ref()
                .and_then(|custom| custom.name.as_deref())
        })
        .unwrap_or_default();
    let existing = delta
        .index
        .and_then(|index| state.tool_by_stream_index.get(&index).copied())
        .or_else(|| {
            delta
                .id
                .as_ref()
                .and_then(|id| state.tool_by_id.get(id).copied())
        });
    let index = if let Some(index) = existing {
        index
    } else {
        let index = output.content.len();
        let custom_property = (delta.custom.is_some() && delta.function.is_none()).then(|| {
            grammar_tool_input_properties
                .get(name)
                .cloned()
                .unwrap_or_else(|| "input".to_owned())
        });
        let arguments = custom_property.as_ref().map_or_else(Map::new, |property| {
            Map::from_iter([(property.clone(), Value::String(String::new()))])
        });
        output
            .content
            .push(AssistantContent::ToolCall(ToolCall::new(
                delta.id.clone().unwrap_or_default(),
                name,
                arguments,
            )));
        state.tool_scratch.insert(
            index,
            ToolScratch {
                partial_args: String::new(),
                custom_input: custom_property.map(|property| CustomInputScratch {
                    property,
                    json_buffer: GrammarToolInputJsonBuffer::default(),
                }),
            },
        );
        if let Some(stream_index) = delta.index {
            state.tool_by_stream_index.insert(stream_index, index);
        }
        if let Some(id) = &delta.id {
            state.tool_by_id.insert(id.clone(), index);
        }
        sender
            .send(AssistantMessageEvent::ToolCallStart {
                content_index: index,
                id: delta.id.clone().unwrap_or_default(),
                tool_name: name.to_owned(),
                namespace: None,
            })
            .map_err(CompletionError::display)?;
        index
    };

    if let Some(stream_index) = delta.index {
        state.tool_by_stream_index.insert(stream_index, index);
    }
    if let Some(id) = &delta.id {
        state.tool_by_id.insert(id.clone(), index);
    }
    let Some(AssistantContent::ToolCall(call)) = output.content.get_mut(index) else {
        return Err(CompletionError::new("tool-call block state is invalid"));
    };
    if call.id.is_empty()
        && let Some(id) = &delta.id
    {
        call.id.clone_from(id);
    }
    if call.name.is_empty() && !name.is_empty() {
        call.name = name.to_owned();
    }
    if let Some(pending) = state
        .pending_reasoning_details_by_tool_call_id
        .remove(&call.id)
    {
        call.thought_signature = Some(pending);
    }

    let scratch = state.tool_scratch.entry(index).or_default();
    if delta.custom.is_some() && scratch.custom_input.is_none() && delta.function.is_none() {
        let property = grammar_tool_input_properties
            .get(&call.name)
            .cloned()
            .unwrap_or_else(|| "input".to_owned());
        call.arguments = Value::Object(Map::from_iter([(
            property.clone(),
            Value::String(String::new()),
        )]));
        scratch.custom_input = Some(CustomInputScratch {
            property,
            json_buffer: GrammarToolInputJsonBuffer::default(),
        });
        scratch.partial_args.clear();
    }
    let emitted_delta = if let Some(arguments) = delta
        .function
        .and_then(|function| function.arguments)
        .filter(|arguments| !arguments.is_empty())
    {
        scratch.partial_args.push_str(&arguments);
        call.arguments = parse_streaming_json(Some(&scratch.partial_args));
        arguments
    } else if let Some(input) = delta
        .custom
        .and_then(|custom| custom.input)
        .filter(|input| !input.is_empty())
    {
        let Some(custom) = scratch.custom_input.as_mut() else {
            return Err(CompletionError::new("custom tool-call state is invalid"));
        };
        let current = call
            .arguments
            .get(&custom.property)
            .and_then(Value::as_str)
            .unwrap_or_default();
        let next = format!("{current}{input}");
        let emitted = append_grammar_tool_input_json_delta(
            &mut custom.json_buffer,
            &custom.property,
            &next,
            false,
        )
        .map_err(CompletionError::display)?
        .unwrap_or_default();
        call.arguments = Value::Object(Map::from_iter([(
            custom.property.clone(),
            Value::String(next),
        )]));
        emitted
    } else {
        String::new()
    };
    sender
        .send(AssistantMessageEvent::ToolCallDelta {
            content_index: index,
            delta: emitted_delta,
        })
        .map_err(CompletionError::display)
}

fn finish_blocks(
    sender: &AssistantStreamSender,
    output: &mut AssistantMessage,
    state: &mut StreamingState,
) -> Result<(), CompletionError> {
    for index in 0..output.content.len() {
        match &mut output.content[index] {
            AssistantContent::Text(block) => sender
                .send(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: block.text.clone(),
                    content_signature: block.text_signature.clone(),
                })
                .map_err(CompletionError::display)?,
            AssistantContent::Thinking(block) => sender
                .send(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: block.thinking.clone(),
                    content_signature: block.thinking_signature.clone(),
                    redacted: block.redacted,
                })
                .map_err(CompletionError::display)?,
            AssistantContent::ToolCall(call) => {
                if let Some(scratch) = state.tool_scratch.get_mut(&index) {
                    if let Some(custom) = scratch.custom_input.as_mut() {
                        let current = call
                            .arguments
                            .get(&custom.property)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        if let Some(delta) = append_grammar_tool_input_json_delta(
                            &mut custom.json_buffer,
                            &custom.property,
                            &current,
                            true,
                        )
                        .map_err(CompletionError::display)?
                        {
                            sender
                                .send(AssistantMessageEvent::ToolCallDelta {
                                    content_index: index,
                                    delta,
                                })
                                .map_err(CompletionError::display)?;
                        }
                    } else {
                        call.arguments = parse_streaming_json(Some(&scratch.partial_args));
                    }
                }
                sender
                    .send(AssistantMessageEvent::ToolCallEnd {
                        content_index: index,
                        tool_call: call.clone(),
                    })
                    .map_err(CompletionError::display)?;
            }
        }
    }
    Ok(())
}

fn parse_chunk_usage(raw: &RawUsage, model: &Model) -> Usage {
    let js_or_zero =
        |value: Option<UsageValue>| value.filter(UsageValue::is_truthy).unwrap_or_default();
    let prompt_tokens = js_or_zero(raw.prompt_tokens.clone());
    let cache_read = raw
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens.clone())
        .or_else(|| raw.prompt_cache_hit_tokens.clone())
        .or_else(|| raw.cached_tokens.clone())
        .unwrap_or_default();
    let cache_write = js_or_zero(
        raw.prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cache_write_tokens.clone()),
    );
    let input =
        (prompt_tokens.as_number() - cache_read.as_number() - cache_write.as_number()).max(0.0);
    let output_tokens = js_or_zero(raw.completion_tokens.clone());
    let reasoning = js_or_zero(
        raw.completion_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens.clone()),
    );
    let input_value: UsageValue = input.into();
    let total_tokens = input_value
        .js_add(&output_tokens)
        .js_add(&cache_read)
        .js_add(&cache_write);
    let mut usage = Usage {
        input: input_value,
        output: output_tokens,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: Some(reasoning),
        total_tokens,
        cost: Default::default(),
    };
    calculate_cost(model, &mut usage);
    usage
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        "content_filter" | "network_error" => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {reason}")),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

fn is_open_ai_reasoning_detail(detail: &Value) -> bool {
    let Some(detail) = detail.as_object() else {
        return false;
    };
    let common_valid = detail
        .get("id")
        .is_none_or(|value| value.is_null() || value.is_string())
        && detail.get("format").is_none_or(Value::is_string)
        && detail.get("index").is_none_or(Value::is_number);
    if !common_valid {
        return false;
    }
    match detail.get("type").and_then(Value::as_str) {
        Some("reasoning.summary") => detail.get("summary").is_some_and(Value::is_string),
        Some("reasoning.encrypted") => detail.get("data").is_some_and(Value::is_string),
        Some("reasoning.text") => {
            detail.get("text").is_some_and(Value::is_string)
                && detail
                    .get("signature")
                    .is_none_or(|value| value.is_null() || value.is_string())
        }
        _ => false,
    }
}

fn encrypted_reasoning_detail(detail: &Value) -> Option<(&str, String)> {
    let detail = detail.as_object()?;
    if detail.get("type")?.as_str()? != "reasoning.encrypted" {
        return None;
    }
    let id = detail.get("id")?.as_str().filter(|id| !id.is_empty())?;
    detail
        .get("data")?
        .as_str()
        .filter(|data| !data.is_empty())?;
    serde_json::to_string(detail)
        .ok()
        .map(|encoded| (id, encoded))
}

fn build_params(
    model: &Model,
    context: &Context,
    options: &OpenAICompletionsOptions,
    compat: &ResolvedCompat,
    cache_retention: CacheRetention,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Result<Value, CompletionError> {
    let messages = convert_messages_wire(model, context, compat, grammar_tool_input_properties)?;
    let cache_control = get_compat_cache_control(compat, cache_retention);
    let prompt_cache_key = (((model.base_url.contains("api.openai.com")
        && cache_retention != CacheRetention::None)
        || (cache_retention == CacheRetention::Long && compat.supports_long_cache_retention))
        .then(|| clamp_open_ai_prompt_cache_key(options.stream.session_id.as_deref())))
    .flatten();
    let mut request = WireRequest {
        model: model.id.clone(),
        messages,
        stream: true,
        prompt_cache_key,
        prompt_cache_retention: (cache_retention == CacheRetention::Long
            && compat.supports_long_cache_retention)
            .then(|| "24h".to_owned()),
        stream_options: compat
            .supports_usage_in_streaming
            .then_some(WireStreamOptions {
                include_usage: true,
            }),
        store: compat.supports_store.then_some(false),
        max_tokens: None,
        max_completion_tokens: None,
        temperature: options.stream.temperature,
        tools: None,
        tool_choice: options.tool_choice.clone(),
        dialect: Map::new(),
    };
    if let Some(max_tokens) = options.stream.max_tokens.filter(|value| *value != 0) {
        match compat.max_tokens_field {
            MaxTokensField::MaxTokens => request.max_tokens = Some(max_tokens),
            MaxTokensField::MaxCompletionTokens => {
                request.max_completion_tokens = Some(max_tokens);
            }
        }
    }

    let deferred_tool_names = if compat.deferred_tools_mode == Some(DeferredToolsMode::Kimi) {
        get_deferred_tool_names(&context.messages)
    } else {
        Vec::new()
    };
    let active_tools = context
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .filter(|tool| !deferred_tool_names.contains(&tool.name))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !active_tools.is_empty() {
        request.tools = Some(convert_tools(&active_tools, compat)?);
        if compat.zai_tool_stream {
            request
                .dialect
                .insert("tool_stream".to_owned(), Value::Bool(true));
        }
    } else if has_tool_history(&context.messages) {
        request.tools = Some(Vec::new());
    }
    if let Some(cache_control) = cache_control {
        apply_anthropic_cache_control(
            &mut request.messages,
            request.tools.as_mut(),
            &cache_control,
        );
    }

    let thinking_budget = resolve_clamped_thinking_budget(model, options, &request);
    apply_thinking_parameters(
        model,
        options,
        compat,
        thinking_budget,
        &mut request.dialect,
    )?;
    if let Some(field) = resolve_thinking_token_budget_field(compat)
        && let Some(thinking_budget) = thinking_budget
    {
        request.dialect.insert(
            thinking_budget_field_name(field).to_owned(),
            crate::types::js_f64_value(thinking_budget),
        );
    }
    if let Some(routing) = &compat.open_router_routing {
        request.dialect.insert(
            "provider".to_owned(),
            serde_json::to_value(routing).map_err(CompletionError::display)?,
        );
    }
    if let Some(routing) = &compat.vercel_gateway_routing {
        let mut gateway = Map::new();
        if let Some(only) = &routing.only {
            gateway.insert(
                "only".to_owned(),
                serde_json::to_value(only).map_err(CompletionError::display)?,
            );
        }
        if let Some(order) = &routing.order {
            gateway.insert(
                "order".to_owned(),
                serde_json::to_value(order).map_err(CompletionError::display)?,
            );
        }
        if !gateway.is_empty() {
            request.dialect.insert(
                "providerOptions".to_owned(),
                Value::Object(Map::from_iter([(
                    "gateway".to_owned(),
                    Value::Object(gateway),
                )])),
            );
        }
    }

    let mut value = serde_json::to_value(request).map_err(CompletionError::display)?;
    if let (Some(object), Some(sampling)) = (
        value.as_object_mut(),
        options.stream.sampling_params.as_ref(),
    ) {
        object.extend(sampling.clone());
    }
    Ok(value)
}

fn resolve_thinking_token_budget_field(
    compat: &ResolvedCompat,
) -> Option<ThinkingTokenBudgetField> {
    compat.thinking_token_budget_field.or_else(|| {
        (compat.supports_thinking_token_budget == Some(true))
            .then_some(ThinkingTokenBudgetField::ThinkingTokenBudget)
    })
}

fn thinking_budget_field_name(field: ThinkingTokenBudgetField) -> &'static str {
    match field {
        ThinkingTokenBudgetField::ThinkingTokenBudget => "thinking_token_budget",
        ThinkingTokenBudgetField::ThinkingBudget => "thinking_budget",
        ThinkingTokenBudgetField::ThinkingBudgetTokens => "thinking_budget_tokens",
    }
}

fn resolve_clamped_thinking_budget(
    model: &Model,
    options: &OpenAICompletionsOptions,
    request: &WireRequest,
) -> Option<f64> {
    let effort = options.reasoning_effort?;
    if !model.reasoning {
        return None;
    }
    let ceiling = request
        .max_tokens
        .or(request.max_completion_tokens)
        .unwrap_or(model.max_tokens);
    let budget = clamp_thinking_budget_to_answer_room(
        thinking_budget_for_level(effort, options.thinking_budgets.as_ref()),
        ceiling as f64,
    );
    (budget > 0.0).then_some(budget)
}

fn apply_thinking_parameters(
    model: &Model,
    options: &OpenAICompletionsOptions,
    compat: &ResolvedCompat,
    thinking_budget: Option<f64>,
    dialect: &mut Map<String, Value>,
) -> Result<(), CompletionError> {
    if !model.reasoning {
        return Ok(());
    }
    let effort = options.reasoning_effort;
    match compat.thinking_format {
        ThinkingFormat::Zai => {
            let thinking = if effort.is_some() {
                serde_json::json!({"type":"enabled", "clear_thinking":false})
            } else {
                serde_json::json!({"type":"disabled"})
            };
            dialect.insert("thinking".to_owned(), thinking);
            if compat.supports_reasoning_effort
                && let Some(mapped) = resolve_defined_effort(model, effort)
            {
                dialect.insert("reasoning_effort".to_owned(), Value::String(mapped));
            }
        }
        ThinkingFormat::Qwen => {
            dialect.insert("enable_thinking".to_owned(), Value::Bool(effort.is_some()));
            if compat.supports_reasoning_effort
                && let Some(mapped) = effort.map(|effort| resolve_nullish_effort(model, effort))
            {
                dialect.insert("reasoning_effort".to_owned(), Value::String(mapped));
            }
        }
        ThinkingFormat::QwenChatTemplate => {
            dialect.insert(
                "chat_template_kwargs".to_owned(),
                serde_json::json!({"enable_thinking": effort.is_some(), "preserve_thinking": true}),
            );
        }
        ThinkingFormat::ChatTemplate => {
            if let Some(values) = build_chat_template_values(
                model,
                options,
                &compat.chat_template_kwargs,
                thinking_budget,
            )? {
                dialect.insert("chat_template_kwargs".to_owned(), Value::Object(values));
            }
        }
        ThinkingFormat::Baseten => {
            if let Some(values) = build_chat_template_values(
                model,
                options,
                &compat.chat_template_args,
                thinking_budget,
            )? {
                dialect.insert("chat_template_args".to_owned(), Value::Object(values));
            }
            let mapped = if let Some(effort) = effort {
                resolve_defined_effort(model, Some(effort))
            } else {
                mapped_off(model)
            };
            if compat.supports_reasoning_effort
                && let Some(mapped) = mapped
            {
                dialect.insert("reasoning_effort".to_owned(), Value::String(mapped));
            }
        }
        ThinkingFormat::Deepseek => {
            if effort.is_some() {
                dialect.insert("thinking".to_owned(), serde_json::json!({"type":"enabled"}));
            } else if !off_is_null(model) {
                dialect.insert(
                    "thinking".to_owned(),
                    serde_json::json!({"type":"disabled"}),
                );
            }
            if compat.supports_reasoning_effort
                && let Some(mapped) = effort.map(|effort| resolve_nullish_effort(model, effort))
            {
                dialect.insert("reasoning_effort".to_owned(), Value::String(mapped));
            }
        }
        ThinkingFormat::Openrouter => {
            if let Some(effort) = effort {
                let mapped = resolve_nullish_effort(model, effort);
                dialect.insert("reasoning".to_owned(), serde_json::json!({"effort":mapped}));
            } else if !off_is_null(model) {
                dialect.insert(
                    "reasoning".to_owned(),
                    serde_json::json!({"effort": mapped_off(model).unwrap_or_else(|| "none".to_owned())}),
                );
            }
        }
        ThinkingFormat::AntLing => {
            if let Some(mapped) = effort.and_then(|effort| mapped_effort_explicit(model, effort)) {
                dialect.insert("reasoning".to_owned(), serde_json::json!({"effort":mapped}));
            }
        }
        ThinkingFormat::Together => {
            dialect.insert(
                "reasoning".to_owned(),
                serde_json::json!({"enabled":effort.is_some()}),
            );
            if compat.supports_reasoning_effort
                && let Some(mapped) = effort.map(|effort| resolve_nullish_effort(model, effort))
            {
                dialect.insert("reasoning_effort".to_owned(), Value::String(mapped));
            }
        }
        ThinkingFormat::StringThinking => {
            if let Some(effort) = effort {
                dialect.insert(
                    "thinking".to_owned(),
                    Value::String(resolve_nullish_effort(model, effort)),
                );
            } else if !off_is_null(model) {
                dialect.insert(
                    "thinking".to_owned(),
                    Value::String(mapped_off(model).unwrap_or_else(|| "none".to_owned())),
                );
            }
        }
        ThinkingFormat::Openai => {
            if compat.supports_reasoning_effort {
                if let Some(effort) = effort {
                    dialect.insert(
                        "reasoning_effort".to_owned(),
                        Value::String(resolve_nullish_effort(model, effort)),
                    );
                } else if let Some(off) = mapped_off(model) {
                    dialect.insert("reasoning_effort".to_owned(), Value::String(off));
                }
            }
        }
    }
    Ok(())
}

fn effort_name(effort: ThinkingLevel) -> &'static str {
    match effort {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn mapped_effort_explicit(model: &Model, effort: ThinkingLevel) -> Option<String> {
    let map = model.thinking_level_map.as_ref()?;
    match effort {
        ThinkingLevel::Minimal => map.minimal.as_ref(),
        ThinkingLevel::Low => map.low.as_ref(),
        ThinkingLevel::Medium => map.medium.as_ref(),
        ThinkingLevel::High => map.high.as_ref(),
        ThinkingLevel::Xhigh => map.xhigh.as_ref(),
        ThinkingLevel::Max => map.max.as_ref(),
    }
    .and_then(Clone::clone)
}

fn effort_mapping(model: &Model, effort: ThinkingLevel) -> Option<&Option<String>> {
    let map = model.thinking_level_map.as_ref()?;
    match effort {
        ThinkingLevel::Minimal => map.minimal.as_ref(),
        ThinkingLevel::Low => map.low.as_ref(),
        ThinkingLevel::Medium => map.medium.as_ref(),
        ThinkingLevel::High => map.high.as_ref(),
        ThinkingLevel::Xhigh => map.xhigh.as_ref(),
        ThinkingLevel::Max => map.max.as_ref(),
    }
}

fn resolve_defined_effort(model: &Model, effort: Option<ThinkingLevel>) -> Option<String> {
    let effort = effort?;
    match effort_mapping(model, effort) {
        None => Some(effort_name(effort).to_owned()),
        Some(Some(mapped)) => Some(mapped.clone()),
        Some(None) => None,
    }
}

fn resolve_nullish_effort(model: &Model, effort: ThinkingLevel) -> String {
    effort_mapping(model, effort)
        .and_then(Clone::clone)
        .unwrap_or_else(|| effort_name(effort).to_owned())
}

fn mapped_off(model: &Model) -> Option<String> {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.off.as_ref())
        .and_then(Clone::clone)
}

fn off_is_null(model: &Model) -> bool {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.off.as_ref())
        == Some(&None)
}

fn build_chat_template_values(
    model: &Model,
    options: &OpenAICompletionsOptions,
    values: &BTreeMap<String, ChatTemplateKwargValue>,
    thinking_budget: Option<f64>,
) -> Result<Option<Map<String, Value>>, CompletionError> {
    let mut resolved = Map::new();
    for (key, value) in values {
        if let Some(value) = resolve_chat_template_value(model, options, value, thinking_budget)? {
            resolved.insert(key.clone(), value);
        }
    }
    Ok((!resolved.is_empty()).then_some(resolved))
}

fn resolve_chat_template_value(
    model: &Model,
    options: &OpenAICompletionsOptions,
    value: &ChatTemplateKwargValue,
    thinking_budget: Option<f64>,
) -> Result<Option<Value>, CompletionError> {
    match value {
        ChatTemplateKwargValue::String(value) => Ok(Some(Value::String(value.clone()))),
        ChatTemplateKwargValue::Number(value) => Ok(Some(Value::Number(value.clone()))),
        ChatTemplateKwargValue::Boolean(value) => Ok(Some(Value::Bool(*value))),
        ChatTemplateKwargValue::Null => Ok(Some(Value::Null)),
        ChatTemplateKwargValue::Variable {
            variable,
            omit_when_off,
        } => {
            if options.reasoning_effort.is_none() && *omit_when_off == Some(true) {
                return Ok(None);
            }
            match variable {
                ThinkingVariable::Enabled => {
                    Ok(Some(Value::Bool(options.reasoning_effort.is_some())))
                }
                ThinkingVariable::Budget => Ok(thinking_budget.map(crate::types::js_f64_value)),
                ThinkingVariable::Effort => {
                    Ok(resolve_defined_effort(model, options.reasoning_effort)
                        .or_else(|| {
                            options
                                .reasoning_effort
                                .is_none()
                                .then(|| mapped_off(model))
                                .flatten()
                        })
                        .map(Value::String))
                }
            }
        }
    }
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(message) => message
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_))),
        Message::User(_) => false,
    })
}

fn get_deferred_tool_names(messages: &[Message]) -> Vec<String> {
    let mut names = Vec::new();
    for name in messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(message) => message.added_tool_names.as_ref(),
            Message::User(_) | Message::Assistant(_) => None,
        })
        .flatten()
    {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

fn get_tools_by_name<'a>(tools: Option<&'a [Tool]>, names: &[String]) -> Vec<&'a Tool> {
    let by_name = tools
        .unwrap_or_default()
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect::<BTreeMap<_, _>>();
    names
        .iter()
        .filter_map(|name| by_name.get(name.as_str()).copied())
        .collect()
}

fn normalize_tool_call_id(id: &str, model: &Model) -> String {
    normalize_open_ai_completions_tool_call_id(id, model)
}

pub fn convert_messages(
    model: &Model,
    context: &Context,
) -> Result<Vec<Value>, OpenAICompletionsBuildError> {
    let compat = get_compat(model);
    let grammar_properties = create_grammar_tool_input_properties(
        context.tools.as_deref(),
        compat.supports_open_ai_grammar_tools,
    )
    .map_err(|error| OpenAICompletionsBuildError(error.to_string()))?;
    convert_messages_wire(model, context, &compat, &grammar_properties)
        .and_then(|messages| {
            messages
                .into_iter()
                .map(|message| serde_json::to_value(message).map_err(CompletionError::display))
                .collect()
        })
        .map_err(|error| OpenAICompletionsBuildError(error.message))
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct OpenAICompletionsBuildError(String);

fn convert_messages_wire(
    model: &Model,
    context: &Context,
    compat: &ResolvedCompat,
    grammar_tool_input_properties: &BTreeMap<String, String>,
) -> Result<Vec<WireMessage>, CompletionError> {
    let normalizer =
        |id: &str, target: &Model, _: &AssistantMessage| normalize_tool_call_id(id, target);
    let transformed = transform_messages(&context.messages, model, Some(&normalizer));
    let mut params = Vec::new();
    if let Some(system_prompt) = &context.system_prompt {
        let content = WireContent::String(sanitize_surrogates(system_prompt));
        if model.reasoning && compat.supports_developer_role {
            params.push(WireMessage::Developer { content });
        } else {
            params.push(WireMessage::System {
                content: Some(content),
                tools: None,
            });
        }
    }

    let mut last_role: Option<&'static str> = None;
    let mut index = 0;
    while index < transformed.len() {
        let message = &transformed[index];
        if compat.requires_assistant_after_tool_result
            && last_role == Some("toolResult")
            && matches!(message, Message::User(_))
        {
            params.push(WireMessage::Assistant {
                content: Some(WireContent::String(
                    "I have processed the tool results.".to_owned(),
                )),
                tool_calls: None,
                reasoning: None,
                reasoning_content: None,
                reasoning_text: None,
                reasoning_details: None,
            });
        }
        match message {
            Message::User(message) => {
                let content = match &message.content {
                    crate::types::UserContent::Text(text) => {
                        WireContent::String(sanitize_surrogates(text))
                    }
                    crate::types::UserContent::Blocks(blocks) => {
                        let parts = blocks
                            .iter()
                            .map(|block| match block {
                                crate::types::UserContentBlock::Text(text) => {
                                    WireContentPart::Text {
                                        text: sanitize_surrogates(&text.text),
                                        cache_control: None,
                                    }
                                }
                                crate::types::UserContentBlock::Image(image) => {
                                    WireContentPart::ImageUrl {
                                        image_url: WireImageUrl {
                                            url: data_url(image),
                                        },
                                    }
                                }
                            })
                            .collect::<Vec<_>>();
                        if parts.is_empty() {
                            index += 1;
                            continue;
                        }
                        WireContent::Parts(parts)
                    }
                };
                params.push(WireMessage::User { content });
                last_role = Some("user");
            }
            Message::Assistant(message) => {
                let text_parts = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Text(block) if !block.text.trim().is_empty() => {
                            Some(WireContentPart::Text {
                                text: sanitize_surrogates(&block.text),
                                cache_control: None,
                            })
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let assistant_text = text_parts
                    .iter()
                    .filter_map(|part| match part {
                        WireContentPart::Text { text, .. } => Some(text.as_str()),
                        WireContentPart::ImageUrl { .. } => None,
                    })
                    .collect::<String>();
                let thinking = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Thinking(block) if !block.thinking.trim().is_empty() => {
                            Some(block)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let mut content = if compat.requires_assistant_after_tool_result {
                    Some(WireContent::String(String::new()))
                } else {
                    None
                };
                let mut reasoning = None;
                let mut reasoning_content = None;
                let mut reasoning_text = None;
                if !thinking.is_empty() {
                    if compat.requires_thinking_as_text {
                        let thinking_text = thinking
                            .iter()
                            .map(|block| sanitize_surrogates(&block.thinking))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let mut combined = vec![WireContentPart::Text {
                            text: thinking_text,
                            cache_control: None,
                        }];
                        combined.extend(text_parts);
                        content = Some(WireContent::Parts(combined));
                    } else {
                        if !assistant_text.is_empty() {
                            content = Some(WireContent::String(assistant_text));
                        }
                        let mut signature = thinking[0].thinking_signature.as_deref();
                        if model.provider.as_str() == "opencode-go"
                            && signature == Some("reasoning")
                        {
                            signature = Some("reasoning_content");
                        }
                        let joined = thinking
                            .iter()
                            .map(|block| block.thinking.as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                        match signature {
                            Some("reasoning") => reasoning = Some(joined),
                            Some("reasoning_content") => reasoning_content = Some(joined),
                            Some("reasoning_text") => reasoning_text = Some(joined),
                            _ => {}
                        }
                    }
                } else if !assistant_text.is_empty() {
                    content = Some(WireContent::String(assistant_text));
                }

                let same_model_reasoning_details = (message.provider == model.provider
                    && message.api == model.api
                    && message.model == model.id)
                    .then(|| message.reasoning_details.clone())
                    .flatten()
                    .filter(|details| !details.is_empty());
                let tool_calls = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(call) => Some(call),
                        _ => None,
                    })
                    .map(|call| {
                        if let Some(property) = grammar_tool_input_properties.get(&call.name) {
                            Ok(WireAssistantToolCall::Custom {
                                id: call.id.clone(),
                                custom: WireCustomCall {
                                    name: call.name.clone(),
                                    input: sanitize_surrogates(
                                        &get_grammar_tool_input(
                                            &call.name,
                                            &call.arguments,
                                            property,
                                        )
                                        .map_err(CompletionError::display)?,
                                    ),
                                },
                            })
                        } else {
                            Ok(WireAssistantToolCall::Function {
                                id: call.id.clone(),
                                function: WireFunctionCall {
                                    name: call.name.clone(),
                                    arguments: serde_json::to_string(&call.arguments)
                                        .map_err(CompletionError::display)?,
                                },
                            })
                        }
                    })
                    .collect::<Result<Vec<_>, CompletionError>>()?;
                let tool_calls = (!tool_calls.is_empty()).then_some(tool_calls);
                let reasoning_details = same_model_reasoning_details.or_else(|| {
                    tool_calls.as_ref().and_then(|_| {
                        let details = message
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                AssistantContent::ToolCall(call) => {
                                    call.thought_signature.as_deref()
                                }
                                _ => None,
                            })
                            .filter_map(|signature| serde_json::from_str::<Value>(signature).ok())
                            .filter(is_open_ai_reasoning_detail)
                            .collect::<Vec<_>>();
                        (!details.is_empty()).then_some(details)
                    })
                });
                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && reasoning_content.is_none()
                {
                    reasoning_content = Some(String::new());
                }
                let has_content = match &content {
                    Some(WireContent::String(content)) => !content.is_empty(),
                    Some(WireContent::Parts(content)) => !content.is_empty(),
                    None => false,
                };
                if !has_content && tool_calls.is_none() {
                    index += 1;
                    continue;
                }
                params.push(WireMessage::Assistant {
                    content,
                    tool_calls,
                    reasoning,
                    reasoning_content,
                    reasoning_text,
                    reasoning_details,
                });
                last_role = Some("assistant");
            }
            Message::ToolResult(_) => {
                let mut image_parts = Vec::new();
                let mut deferred_tool_names = Vec::new();
                let mut next = index;
                while next < transformed.len() {
                    let Message::ToolResult(tool_result) = &transformed[next] else {
                        break;
                    };
                    append_tool_result_message(
                        &mut params,
                        tool_result,
                        model,
                        compat,
                        &mut image_parts,
                        &mut deferred_tool_names,
                    );
                    next += 1;
                }
                index = next.saturating_sub(1);
                if !image_parts.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        params.push(WireMessage::Assistant {
                            content: Some(WireContent::String(
                                "I have processed the tool results.".to_owned(),
                            )),
                            tool_calls: None,
                            reasoning: None,
                            reasoning_content: None,
                            reasoning_text: None,
                            reasoning_details: None,
                        });
                    }
                    let mut parts = vec![WireContentPart::Text {
                        text: "Attached image(s) from tool result:".to_owned(),
                        cache_control: None,
                    }];
                    parts.extend(image_parts);
                    params.push(WireMessage::User {
                        content: WireContent::Parts(parts),
                    });
                    last_role = Some("user");
                } else {
                    last_role = Some("toolResult");
                }
                if !deferred_tool_names.is_empty() {
                    let tools = get_tools_by_name(context.tools.as_deref(), &deferred_tool_names)
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>();
                    if !tools.is_empty() {
                        params.push(WireMessage::System {
                            content: None,
                            tools: Some(convert_tools(&tools, compat)?),
                        });
                    }
                }
            }
        }
        index += 1;
    }
    Ok(params)
}

fn append_tool_result_message(
    params: &mut Vec<WireMessage>,
    tool_result: &ToolResultMessage,
    model: &Model,
    compat: &ResolvedCompat,
    image_parts: &mut Vec<WireContentPart>,
    deferred_tool_names: &mut Vec<String>,
) {
    let text = tool_result
        .content
        .iter()
        .filter_map(|block| match block {
            crate::types::UserContentBlock::Text(block) => Some(block.text.as_str()),
            crate::types::UserContentBlock::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let has_images = tool_result
        .content
        .iter()
        .any(|block| matches!(block, crate::types::UserContentBlock::Image(_)));
    let content = if !text.is_empty() {
        text
    } else if has_images {
        "(see attached image)".to_owned()
    } else {
        "(no tool output)".to_owned()
    };
    params.push(WireMessage::Tool {
        content: WireContent::String(sanitize_surrogates(&content)),
        tool_call_id: tool_result.tool_call_id.clone(),
        name: (compat.requires_tool_result_name && !tool_result.tool_name.is_empty())
            .then(|| tool_result.tool_name.clone()),
    });
    if compat.deferred_tools_mode == Some(DeferredToolsMode::Kimi) {
        for name in tool_result.added_tool_names.iter().flatten() {
            if !deferred_tool_names.contains(name) {
                deferred_tool_names.push(name.clone());
            }
        }
    }
    if has_images && model.input.contains(&ModelInput::Image) {
        image_parts.extend(tool_result.content.iter().filter_map(|block| match block {
            crate::types::UserContentBlock::Image(image) => Some(WireContentPart::ImageUrl {
                image_url: WireImageUrl {
                    url: data_url(image),
                },
            }),
            crate::types::UserContentBlock::Text(_) => None,
        }));
    }
}

fn data_url(image: &ImageContent) -> String {
    format!("data:{};base64,{}", image.mime_type, image.data)
}

fn convert_tools(
    tools: &[Tool],
    compat: &ResolvedCompat,
) -> Result<Vec<WireTool>, CompletionError> {
    tools
        .iter()
        .map(|tool| {
            if let Some(grammar) =
                resolve_grammar_constrained_sampling(tool, compat.supports_open_ai_grammar_tools)
                    .map_err(CompletionError::display)?
            {
                let syntax = match grammar.format {
                    GrammarConstrainedSamplingFormat::Lark => GrammarSyntax::Lark,
                    GrammarConstrainedSamplingFormat::Regex => GrammarSyntax::Regex,
                };
                return Ok(WireTool {
                    definition: WireToolDefinition::Custom {
                        custom: WireCustomDefinition {
                            name: tool.name.clone(),
                            description: tool.description.clone(),
                            format: WireCustomFormat {
                                kind: GrammarType::Grammar,
                                grammar: WireGrammar {
                                    syntax,
                                    definition: grammar.definition,
                                },
                            },
                        },
                    },
                    cache_control: None,
                });
            }
            let strict = resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode)
                .map_err(CompletionError::display)?;
            let parameters =
                get_json_schema_tool_parameters(tool, strict).map_err(CompletionError::display)?;
            Ok(WireTool {
                definition: WireToolDefinition::Function {
                    function: WireFunctionDefinition {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        parameters,
                        strict: compat
                            .supports_strict_mode
                            .then_some(strict.unwrap_or(false)),
                    },
                },
                cache_control: None,
            })
        })
        .collect()
}

fn get_compat_cache_control(
    compat: &ResolvedCompat,
    retention: CacheRetention,
) -> Option<CompatCacheControl> {
    if compat.cache_control_format != Some(CacheControlFormat::Anthropic)
        || retention == CacheRetention::None
    {
        return None;
    }
    Some(CompatCacheControl {
        kind: CacheControlType::Ephemeral,
        ttl: (retention == CacheRetention::Long && compat.supports_long_cache_retention)
            .then(|| "1h".to_owned()),
    })
}

fn apply_anthropic_cache_control(
    messages: &mut [WireMessage],
    tools: Option<&mut Vec<WireTool>>,
    cache_control: &CompatCacheControl,
) {
    for message in messages.iter_mut() {
        match message {
            WireMessage::System { content, .. } => {
                if let Some(content) = content {
                    add_cache_control_to_content(content, cache_control);
                }
                break;
            }
            WireMessage::Developer { content } => {
                add_cache_control_to_content(content, cache_control);
                break;
            }
            WireMessage::User { .. } | WireMessage::Assistant { .. } | WireMessage::Tool { .. } => {
            }
        }
    }
    if let Some(tool) = tools.and_then(|tools| tools.last_mut()) {
        tool.cache_control = Some(cache_control.clone());
    }
    for message in messages.iter_mut().rev() {
        let content = match message {
            WireMessage::User { content } | WireMessage::Tool { content, .. } => Some(content),
            WireMessage::Assistant { content, .. } => content.as_mut(),
            WireMessage::System { .. } | WireMessage::Developer { .. } => None,
        };
        if let Some(content) = content
            && add_cache_control_to_content(content, cache_control)
        {
            break;
        }
    }
}

fn add_cache_control_to_content(
    content: &mut WireContent,
    cache_control: &CompatCacheControl,
) -> bool {
    match content {
        WireContent::String(text) => {
            if text.is_empty() {
                return false;
            }
            *content = WireContent::Parts(vec![WireContentPart::Text {
                text: text.clone(),
                cache_control: Some(cache_control.clone()),
            }]);
            true
        }
        WireContent::Parts(parts) => {
            for part in parts.iter_mut().rev() {
                if let WireContentPart::Text {
                    cache_control: marker,
                    ..
                } = part
                {
                    *marker = Some(cache_control.clone());
                    return true;
                }
            }
            false
        }
    }
}

fn detect_compat(model: &Model) -> ResolvedCompat {
    let provider = model.provider.as_str();
    let base_url = model.base_url.as_str();
    let lower_base_url = base_url.to_ascii_lowercase();
    let is_zai = matches!(provider, "zai" | "zai-coding-cn")
        || base_url.contains("api.z.ai")
        || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai")
        || base_url.contains("api.together.xyz");
    let is_moonshot =
        matches!(provider, "moonshotai" | "moonshotai-cn") || base_url.contains("api.moonshot.");
    let is_open_router = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cloudflare_workers =
        provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cloudflare_gateway =
        provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_deepseek = provider == "deepseek" || lower_base_url.contains("deepseek.com");
    let is_non_standard = is_nvidia
        || provider == "cerebras"
        || base_url.contains("cerebras.ai")
        || provider == "xai"
        || base_url.contains("api.x.ai")
        || is_together
        || base_url.contains("chutes.ai")
        || is_deepseek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers
        || is_cloudflare_gateway
        || is_ant_ling;
    let use_max_tokens = base_url.contains("chutes.ai")
        || is_deepseek
        || is_moonshot
        || is_cloudflare_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;
    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let open_router_developer_role =
        is_open_router && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
    ResolvedCompat {
        supports_store: !is_non_standard,
        supports_developer_role: open_router_developer_role
            || (!is_non_standard && !is_open_router),
        supports_reasoning_effort: !is_grok
            && !is_zai
            && !is_moonshot
            && !is_together
            && !is_cloudflare_gateway
            && !is_nvidia
            && !is_ant_ling,
        supports_usage_in_streaming: true,
        supports_finish_reason: true,
        max_tokens_field: if use_max_tokens {
            MaxTokensField::MaxTokens
        } else {
            MaxTokensField::MaxCompletionTokens
        },
        requires_tool_result_name: false,
        requires_assistant_after_tool_result: false,
        requires_thinking_as_text: false,
        requires_reasoning_content_on_assistant_messages: is_deepseek,
        thinking_format: if is_deepseek {
            ThinkingFormat::Deepseek
        } else if is_zai {
            ThinkingFormat::Zai
        } else if is_together {
            ThinkingFormat::Together
        } else if is_ant_ling {
            ThinkingFormat::AntLing
        } else if is_open_router {
            ThinkingFormat::Openrouter
        } else {
            ThinkingFormat::Openai
        },
        chat_template_kwargs: BTreeMap::new(),
        chat_template_args: BTreeMap::new(),
        open_router_routing: None,
        vercel_gateway_routing: None,
        zai_tool_stream: false,
        thinking_token_budget_field: None,
        supports_thinking_token_budget: Some(false),
        supports_open_ai_grammar_tools: false,
        supports_strict_mode: !is_moonshot && !is_together && !is_cloudflare_gateway && !is_nvidia,
        cache_control_format: (provider == "openrouter" && model.id.starts_with("anthropic/"))
            .then_some(CacheControlFormat::Anthropic),
        send_session_affinity_headers: false,
        deferred_tools_mode: None,
        session_affinity_format: if is_open_router {
            SessionAffinityFormat::Openrouter
        } else {
            SessionAffinityFormat::Openai
        },
        supports_long_cache_retention: !(is_together
            || is_cloudflare_workers
            || is_cloudflare_gateway
            || is_nvidia
            || is_ant_ling),
    }
}

fn get_compat(model: &Model) -> ResolvedCompat {
    let mut resolved = detect_compat(model);
    let Some(compat) = model.compat.as_ref() else {
        return resolved;
    };
    let compat = serde_json::to_value(compat)
        .ok()
        .and_then(|value| serde_json::from_value::<OpenAICompletionsCompat>(value).ok())
        .unwrap_or_default();
    apply_compat(&mut resolved, &compat);
    resolved
}

fn apply_compat(resolved: &mut ResolvedCompat, compat: &OpenAICompletionsCompat) {
    macro_rules! assign {
        ($field:ident) => {
            if let Some(value) = compat.$field {
                resolved.$field = value;
            }
        };
    }
    assign!(supports_store);
    assign!(supports_developer_role);
    assign!(supports_reasoning_effort);
    assign!(supports_usage_in_streaming);
    assign!(supports_finish_reason);
    assign!(max_tokens_field);
    assign!(requires_tool_result_name);
    assign!(requires_assistant_after_tool_result);
    assign!(requires_thinking_as_text);
    assign!(requires_reasoning_content_on_assistant_messages);
    assign!(thinking_format);
    assign!(zai_tool_stream);
    assign!(supports_open_ai_grammar_tools);
    assign!(supports_strict_mode);
    assign!(send_session_affinity_headers);
    assign!(session_affinity_format);
    assign!(supports_long_cache_retention);
    resolved.chat_template_kwargs = compat
        .chat_template_kwargs
        .clone()
        .unwrap_or_else(|| resolved.chat_template_kwargs.clone());
    resolved.chat_template_args = compat
        .chat_template_args
        .clone()
        .unwrap_or_else(|| resolved.chat_template_args.clone());
    resolved.open_router_routing = compat
        .open_router_routing
        .clone()
        .or_else(|| resolved.open_router_routing.clone());
    resolved.vercel_gateway_routing = compat
        .vercel_gateway_routing
        .clone()
        .or_else(|| resolved.vercel_gateway_routing.clone());
    if compat.thinking_token_budget_field.is_some() {
        resolved.thinking_token_budget_field = compat.thinking_token_budget_field;
    }
    if compat.supports_thinking_token_budget.is_some() {
        resolved.supports_thinking_token_budget = compat.supports_thinking_token_budget;
    }
    if compat.cache_control_format.is_some() {
        resolved.cache_control_format = compat.cache_control_format;
    }
    if compat.deferred_tools_mode.is_some() {
        resolved.deferred_tools_mode = compat.deferred_tools_mode;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AbortSignal, FetchFunction, ModelCost, ModelCostRates, ProviderHttpRequest,
        ProviderHttpResponse, ProviderResponse, ThinkingLevelMap, ToolResultRole, UserContent,
        UserMessage, UserRole,
    };
    use bytes::Bytes;
    use futures::StreamExt;
    use http_body_util::{BodyExt, StreamBody};
    use hyper::body::{Frame, Incoming};
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio::task::JoinHandle;

    #[derive(Clone)]
    struct Script {
        status: StatusCode,
        headers: BTreeMap<String, String>,
        body: Vec<Bytes>,
    }

    impl Script {
        fn sse(chunks: Vec<Value>) -> Self {
            let mut body = chunks
                .into_iter()
                .map(|chunk| format!("data: {chunk}\n\n"))
                .collect::<String>();
            body.push_str("data: [DONE]\n\n");
            Self {
                status: StatusCode::OK,
                headers: BTreeMap::new(),
                body: vec![Bytes::from(body)],
            }
        }

        fn raw_sse(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                status: StatusCode::OK,
                headers: BTreeMap::new(),
                body: chunks.into_iter().map(Bytes::from).collect(),
            }
        }

        fn error(status: StatusCode, message: &str) -> Self {
            Self {
                status,
                headers: BTreeMap::new(),
                body: vec![Bytes::from(
                    json!({"error":{"message":message,"type":"test","param":null,"code":null}})
                        .to_string(),
                )],
            }
        }

        fn raw_error(status: StatusCode, body: impl Into<Bytes>) -> Self {
            Self {
                status,
                headers: BTreeMap::new(),
                body: vec![body.into()],
            }
        }
    }

    struct StaticFetch(Vec<u8>);

    impl FetchFunction for StaticFetch {
        fn fetch(
            &self,
            _request: ProviderHttpRequest,
        ) -> futures::future::BoxFuture<'_, Result<ProviderHttpResponse, String>> {
            let body = self.0.clone();
            Box::pin(async move {
                Ok(ProviderHttpResponse {
                    status: 200,
                    status_text: "OK".to_owned(),
                    headers: BTreeMap::new(),
                    body: Some(futures::stream::iter(vec![Ok(body)]).boxed()),
                })
            })
        }
    }

    struct PendingStaticFetch(Vec<u8>);

    impl FetchFunction for PendingStaticFetch {
        fn fetch(
            &self,
            _request: ProviderHttpRequest,
        ) -> futures::future::BoxFuture<'_, Result<ProviderHttpResponse, String>> {
            let body = self.0.clone();
            Box::pin(async move {
                Ok(ProviderHttpResponse {
                    status: 200,
                    status_text: "OK".to_owned(),
                    headers: BTreeMap::new(),
                    body: Some(
                        futures::stream::iter(vec![Ok(body)])
                            .chain(futures::stream::pending())
                            .boxed(),
                    ),
                })
            })
        }
    }

    #[derive(Default)]
    struct RecordingFetch {
        calls: AtomicUsize,
    }

    impl FetchFunction for RecordingFetch {
        fn fetch(
            &self,
            _request: ProviderHttpRequest,
        ) -> futures::future::BoxFuture<'_, Result<ProviderHttpResponse, String>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err("unexpected request".to_owned()) })
        }
    }

    #[derive(Default)]
    struct ManualAbort {
        aborted: AtomicBool,
        notify: Notify,
    }

    impl ManualAbort {
        fn abort(&self) {
            self.aborted.store(true, Ordering::Release);
            self.notify.notify_waiters();
        }
    }

    impl AbortSignal for ManualAbort {
        fn is_aborted(&self) -> bool {
            self.aborted.load(Ordering::Acquire)
        }

        fn cancelled(&self) -> futures::future::BoxFuture<'_, ()> {
            Box::pin(async move {
                while !self.is_aborted() {
                    self.notify.notified().await;
                }
            })
        }
    }

    #[derive(Debug, Clone)]
    struct CapturedRequest {
        headers: BTreeMap<String, String>,
        header_lines: Vec<(String, String)>,
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

    async fn start_server(scripts: Vec<Script>, direct_open_ai: bool) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let scripts = Arc::new(Mutex::new(scripts.into_iter()));
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
                            let header_lines = request
                                .headers()
                                .keys()
                                .flat_map(|name| {
                                    request.headers().get_all(name).iter().filter_map(|value| {
                                        value.to_str().ok().map(|value| {
                                            (name.as_str().to_owned(), value.to_owned())
                                        })
                                    })
                                })
                                .collect();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            let body = serde_json::from_slice(&bytes).unwrap();
                            captured
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(CapturedRequest {
                                    headers,
                                    header_lines,
                                    body,
                                });
                            let script = scripts
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .next()
                                .unwrap_or_else(|| Script::sse(vec![stop_chunk("test-model")]));
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
                            let frames =
                                futures::stream::iter(script.body.into_iter().map(|bytes| {
                                    Ok::<_, std::convert::Infallible>(Frame::data(bytes))
                                }));
                            Ok::<_, std::convert::Infallible>(
                                response.body(StreamBody::new(frames)).unwrap(),
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        let base_url = if direct_open_ai {
            format!("http://api.openai.com@{address}")
        } else {
            format!("http://{address}")
        };
        TestServer {
            base_url,
            captured,
            task,
        }
    }

    fn stop_chunk(model: &str) -> Value {
        json!({
            "id":"chatcmpl-test",
            "object":"chat.completion.chunk",
            "created":0,
            "model":model,
            "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1}
        })
    }

    fn model(base_url: String) -> Model {
        Model {
            id: "test-model".to_owned(),
            name: "Test Model".to_owned(),
            api: "openai-completions".into(),
            provider: "openai".into(),
            base_url,
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                rates: ModelCostRates::default(),
                tiers: None,
            },
            context_window: 128_000,
            max_tokens: 4_096,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn context() -> Context {
        Context {
            system_prompt: Some("System prompt".to_owned()),
            messages: vec![Message::User(Box::new(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hello".to_owned()),
                timestamp: 1,
            }))],
            tools: None,
        }
    }

    fn options() -> OpenAICompletionsOptions {
        let mut options = OpenAICompletionsOptions::default();
        options.stream.request.api_key = Some("test-key".to_owned());
        options
    }

    fn payload(
        test_model: &Model,
        test_context: &Context,
        request_options: &OpenAICompletionsOptions,
    ) -> Value {
        let compat = get_compat(test_model);
        let grammar_properties = create_grammar_tool_input_properties(
            test_context.tools.as_deref(),
            compat.supports_open_ai_grammar_tools,
        )
        .unwrap();
        build_params(
            test_model,
            test_context,
            request_options,
            &compat,
            resolve_cache_retention(
                request_options.stream.cache_retention,
                request_options.stream.request.env.as_ref(),
            ),
            &grammar_properties,
        )
        .unwrap()
    }

    async fn run_and_capture(
        model: Model,
        context: Context,
        options: OpenAICompletionsOptions,
        server: &TestServer,
    ) -> (AssistantMessage, CapturedRequest) {
        let mut event_stream = stream(&model, &context, options);
        while event_stream.next().await.is_some() {}
        let message = event_stream.result().await.unwrap();
        let captured = server
            .captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last()
            .cloned()
            .unwrap();
        (message, captured)
    }

    async fn run_message(
        model: &Model,
        context: &Context,
        options: OpenAICompletionsOptions,
    ) -> AssistantMessage {
        let mut events = stream(model, context, options);
        while events.next().await.is_some() {}
        events.result().await.unwrap()
    }

    /// Pins pi `src/api/openai-completions.ts:663-683`: thrown provider work is
    /// converted into an in-band terminal error.
    #[tokio::test]
    async fn provider_task_panic_is_terminal_in_band() {
        let mut request_options = options();
        request_options.stream.request.on_payload = Some(Arc::new(|_, _| {
            Box::pin(async { panic!("payload hook panic") })
        }));
        let message = run_message(
            &model("https://example.invalid/v1".to_owned()),
            &context(),
            request_options,
        )
        .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some("payload hook panic"));
    }

    /// Pins pi `src/api/openai-completions.ts:315,663-683`: a rejected hook
    /// becomes the same in-band error as a synchronously thrown hook.
    #[tokio::test]
    async fn rejected_payload_hook_is_terminal_in_band() {
        let mut request_options = options();
        request_options.stream.request.on_payload = Some(Arc::new(|_, _| {
            Box::pin(async { Err("payload hook rejected".to_owned()) })
        }));
        let message = run_message(
            &model("https://example.invalid/v1".to_owned()),
            &context(),
            request_options,
        )
        .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("payload hook rejected")
        );
    }

    fn completions_compat(value: OpenAICompletionsCompat) -> Option<ModelCompat> {
        Some(ModelCompat::OpenAICompletions(Box::new(value)))
    }

    /// Pins the request-dialect branches in openai-completions.ts:806-957 and
    /// the corresponding tool-choice.test.ts compatibility cases without HTTP.
    #[test]
    fn thinking_format_request_matrix_matches_pi_source() {
        let mut test_model = model("https://example.invalid/v1".to_owned());
        test_model.reasoning = true;
        test_model.provider = "custom".into();
        test_model.thinking_level_map = Some(ThinkingLevelMap {
            off: Some(Some("disabled-level".to_owned())),
            high: Some(Some("high-level".to_owned())),
            ..ThinkingLevelMap::default()
        });
        let mut request_options = options();
        request_options.reasoning_effort = Some(ThinkingLevel::High);

        let cases = [
            (
                ThinkingFormat::Zai,
                json!({"thinking":{"type":"enabled","clear_thinking":false},"reasoning_effort":"high-level"}),
            ),
            (
                ThinkingFormat::Qwen,
                json!({"enable_thinking":true,"reasoning_effort":"high-level"}),
            ),
            (
                ThinkingFormat::QwenChatTemplate,
                json!({"chat_template_kwargs":{"enable_thinking":true,"preserve_thinking":true}}),
            ),
            (
                ThinkingFormat::Deepseek,
                json!({"thinking":{"type":"enabled"},"reasoning_effort":"high-level"}),
            ),
            (
                ThinkingFormat::Openrouter,
                json!({"reasoning":{"effort":"high-level"}}),
            ),
            (
                ThinkingFormat::Together,
                json!({"reasoning":{"enabled":true},"reasoning_effort":"high-level"}),
            ),
            (
                ThinkingFormat::StringThinking,
                json!({"thinking":"high-level"}),
            ),
            (
                ThinkingFormat::AntLing,
                json!({"reasoning":{"effort":"high-level"}}),
            ),
            (
                ThinkingFormat::Openai,
                json!({"reasoning_effort":"high-level"}),
            ),
        ];
        for (format, expected) in cases {
            test_model.compat = completions_compat(OpenAICompletionsCompat {
                thinking_format: Some(format),
                supports_reasoning_effort: Some(true),
                ..OpenAICompletionsCompat::default()
            });
            let body = payload(&test_model, &context(), &request_options);
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(&body[key], value, "{format:?} key {key}");
            }
        }

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::Baseten),
            supports_reasoning_effort: Some(true),
            chat_template_args: Some(BTreeMap::from([(
                "thinking".to_owned(),
                ChatTemplateKwargValue::Variable {
                    variable: ThinkingVariable::Enabled,
                    omit_when_off: None,
                },
            )])),
            ..OpenAICompletionsCompat::default()
        });
        let body = payload(&test_model, &context(), &request_options);
        assert_eq!(body["chat_template_args"], json!({"thinking":true}));
        assert_eq!(body["reasoning_effort"], "high-level");

        request_options.reasoning_effort = None;
        let body = payload(&test_model, &context(), &request_options);
        assert_eq!(body["chat_template_args"], json!({"thinking":false}));
        assert_eq!(body["reasoning_effort"], "disabled-level");

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::Deepseek),
            ..OpenAICompletionsCompat::default()
        });
        let body = payload(&test_model, &context(), &request_options);
        assert_eq!(body["thinking"], json!({"type":"disabled"}));

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::Zai),
            supports_reasoning_effort: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        let body = payload(&test_model, &context(), &request_options);
        assert_eq!(body["thinking"], json!({"type":"disabled"}));
        assert!(body.get("reasoning_effort").is_none());

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::StringThinking),
            ..OpenAICompletionsCompat::default()
        });
        assert_eq!(
            payload(&test_model, &context(), &request_options)["thinking"],
            "disabled-level"
        );

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::ChatTemplate),
            chat_template_kwargs: Some(BTreeMap::from([
                (
                    "effort".to_owned(),
                    ChatTemplateKwargValue::Variable {
                        variable: ThinkingVariable::Effort,
                        omit_when_off: None,
                    },
                ),
                (
                    "static".to_owned(),
                    ChatTemplateKwargValue::String("value".to_owned()),
                ),
            ])),
            ..OpenAICompletionsCompat::default()
        });
        assert_eq!(
            payload(&test_model, &context(), &request_options)["chat_template_kwargs"],
            json!({"effort":"disabled-level","static":"value"})
        );

        test_model.thinking_level_map.as_mut().unwrap().off = Some(None);
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::StringThinking),
            ..OpenAICompletionsCompat::default()
        });
        let body = payload(&test_model, &context(), &request_options);
        assert!(body.get("thinking").is_none());

        request_options.reasoning_effort = Some(ThinkingLevel::High);
        test_model.thinking_level_map.as_mut().unwrap().high = Some(None);
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::AntLing),
            ..OpenAICompletionsCompat::default()
        });
        assert!(
            payload(&test_model, &context(), &request_options)
                .get("reasoning")
                .is_none()
        );
        test_model.reasoning = false;
        test_model.thinking_level_map.as_mut().unwrap().high = Some(Some("high-level".to_owned()));
        assert!(
            payload(&test_model, &context(), &request_options)
                .get("reasoning")
                .is_none()
        );
    }

    /// Pins openai-completions.ts:766-804, 959-994 and the strict/tool-stream
    /// payload cases from empty-tools.test.ts and tool-choice.test.ts.
    #[test]
    fn named_fields_tools_routing_and_sampling_precedence_match_pi_source() {
        let mut test_model = model("https://gateway.ai.cloudflare.com/v1/a/g/compat".to_owned());
        test_model.provider = "cloudflare-ai-gateway".into();
        test_model.reasoning = true;
        let mut request_options = options();
        request_options.stream.max_tokens = Some(1_234);
        request_options.reasoning_effort = Some(ThinkingLevel::High);
        let body = payload(&test_model, &context(), &request_options);
        assert_eq!(body["max_tokens"], 1_234);
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("store").is_none());
        assert_eq!(body["messages"][0]["role"], "system");

        test_model.provider = "custom".into();
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            zai_tool_stream: Some(true),
            supports_strict_mode: Some(false),
            open_router_routing: Some(OpenRouterRouting {
                only: Some(vec!["provider-a".to_owned()]),
                ..OpenRouterRouting::default()
            }),
            vercel_gateway_routing: Some(VercelGatewayRouting {
                order: Some(vec!["provider-b".to_owned()]),
                ..VercelGatewayRouting::default()
            }),
            ..OpenAICompletionsCompat::default()
        });
        let mut test_context = context();
        test_context.tools = Some(vec![Tool {
            name: "read".to_owned(),
            description: "Read".to_owned(),
            parameters: json!({"type":"object","properties":{}}),
            constrained_sampling: None,
        }]);
        request_options.stream.sampling_params = Some(Map::from_iter([
            ("model".to_owned(), Value::String("caller-model".to_owned())),
            ("temperature".to_owned(), Value::from(0.25)),
            ("tools".to_owned(), json!([])),
        ]));
        let body = payload(&test_model, &test_context, &request_options);
        assert_eq!(body["model"], "caller-model");
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["tool_stream"], true);
        assert_eq!(body["provider"], json!({"only":["provider-a"]}));
        assert_eq!(
            body["providerOptions"],
            json!({"gateway":{"order":["provider-b"]}})
        );

        request_options.stream.sampling_params = None;
        let body = payload(&test_model, &test_context, &request_options);
        assert!(body["tools"][0]["function"].get("strict").is_none());

        let body = payload(&test_model, &context(), &request_options);
        assert!(body.get("tool_stream").is_none());
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            zai_tool_stream: Some(false),
            ..OpenAICompletionsCompat::default()
        });
        let body = payload(&test_model, &test_context, &request_options);
        assert!(body.get("tool_stream").is_none());
    }

    /// Pins pi `src/types.ts:596-599,717-790` and
    /// `src/api/openai-completions.ts:933-936`: OpenRouter routing is forwarded
    /// verbatim as the request's `provider` field, including unknown nested keys.
    #[test]
    fn unknown_open_router_routing_key_reaches_request_body() {
        let mut test_model = model("https://openrouter.ai/api/v1".to_owned());
        let compat: OpenAICompletionsCompat = serde_json::from_value(json!({
            "openRouterRouting": {
                "only": ["provider-a"],
                "custom_router": {"region":"west","weights":[1,2]}
            }
        }))
        .unwrap();
        test_model.compat = completions_compat(compat);

        let body = payload(&test_model, &context(), &options());
        assert_eq!(
            body["provider"],
            json!({
                "only":["provider-a"],
                "custom_router":{"region":"west","weights":[1,2]}
            })
        );
    }

    /// Pins pi `src/api/openai-completions.ts:798-800` request-number behavior.
    #[test]
    fn whole_temperature_serializes_as_a_json_integer() {
        let test_model = model("https://example.invalid/v1".to_owned());
        let mut request_options = options();
        request_options.stream.temperature = Some(1.0);
        let wire =
            serde_json::to_string(&payload(&test_model, &context(), &request_options)).unwrap();
        assert!(wire.contains(r#""temperature":1"#));
        assert!(!wire.contains(r#""temperature":1.0"#));
    }

    /// Pins Kimi deferred-tool ordering and same-request injection from
    /// openai-completions.ts:76-97, 782-788 and 1377-1392.
    #[test]
    fn kimi_deferred_tools_preserve_first_added_order() {
        let mut test_model = model("https://example.invalid/v1".to_owned());
        test_model.provider = "moonshotai".into();
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            deferred_tools_mode: Some(DeferredToolsMode::Kimi),
            ..OpenAICompletionsCompat::default()
        });
        let tool = |name: &str| Tool {
            name: name.to_owned(),
            description: name.to_owned(),
            parameters: json!({"type":"object","properties":{}}),
            constrained_sampling: None,
        };
        let mut assistant =
            AssistantMessage::pending("openai-completions", "moonshotai", "test-model", 2);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![AssistantContent::ToolCall(ToolCall::new(
            "call-1",
            "bootstrap",
            Map::new(),
        ))]
        .into();
        let tool_result = Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-1".to_owned(),
            tool_name: "bootstrap".to_owned(),
            content: vec![crate::types::UserContentBlock::Text(TextContent::new("ok"))],
            details: None,
            usage: None,
            added_tool_names: Some(vec![
                "zeta".to_owned(),
                "alpha".to_owned(),
                "zeta".to_owned(),
            ]),
            is_error: false,
            timestamp: 3,
        }));
        let test_context = Context {
            system_prompt: None,
            messages: vec![
                context().messages[0].clone(),
                Message::Assistant(Box::new(assistant)),
                tool_result,
            ],
            tools: Some(vec![tool("bootstrap"), tool("alpha"), tool("zeta")]),
        };
        let body = payload(&test_model, &test_context, &options());
        assert_eq!(body["tools"][0]["function"]["name"], "bootstrap");
        let injected = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(injected["role"], "system");
        assert!(injected.get("content").is_none());
        assert_eq!(
            injected["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
    }

    /// Pins pi `src/api/openai-completions.ts:539-566,1491-1509`: wrong-typed
    /// content is ignored while truthy usage values retain JavaScript semantics.
    #[test]
    fn raw_chunk_tolerance_matches_pi_source() {
        let chunk: RawChunk = serde_json::from_value(json!({
            "id":"",
            "choices":[{
                "delta":{"content":[],"reasoning":123},
                "usage":{"prompt_tokens":1.5,"completion_tokens":"2"}
            }],
            "usage":{
                "prompt_tokens":1.5,
                "completion_tokens":"2",
                "prompt_tokens_details":[]
            }
        }))
        .unwrap();
        let usage = chunk.usage.expect("usage object");
        assert_eq!(
            serde_json::to_value(&usage.prompt_tokens).unwrap(),
            json!(1.5)
        );
        assert_eq!(
            serde_json::to_value(&usage.completion_tokens).unwrap(),
            json!("2")
        );
        assert!(usage.prompt_tokens_details.is_none());
        let choice = chunk.choices.first().expect("choice");
        let delta = choice.delta.as_ref().expect("delta object");
        assert_eq!(delta.content, None);
        assert_eq!(delta.reasoning, None);
        let usage = choice.usage.as_ref().expect("choice usage");
        assert_eq!(
            serde_json::to_value(&usage.prompt_tokens).unwrap(),
            json!(1.5)
        );
        assert_eq!(
            serde_json::to_value(&usage.completion_tokens).unwrap(),
            json!("2")
        );
        let parsed = parse_chunk_usage(usage, &model("https://example.invalid/v1".to_owned()));
        let parsed = serde_json::to_value(parsed).expect("usage wire");
        assert_eq!(parsed["input"], 1.5);
        assert_eq!(parsed["output"], "2");
        assert_eq!(parsed["reasoning"], 0);
        assert_eq!(parsed["totalTokens"], "1.5200");

        let chunk: RawChunk = serde_json::from_value(json!({"choices":null,"usage":null})).unwrap();
        assert!(chunk.choices.is_empty());
        assert!(chunk.usage.is_none());
        let delta: RawDelta = serde_json::from_value(json!({"tool_calls":null})).unwrap();
        assert!(delta.tool_calls.is_empty());
    }

    /// Ports the full thinking-token-budget.test.ts payload matrix through the
    /// same private builder exercised by the HTTP capture tests.
    #[test]
    fn thinking_token_budget_payload_matrix_matches_pi() {
        let mut test_model = model("https://example.invalid/v1".to_owned());
        test_model.reasoning = true;
        test_model.max_tokens = 16_384;
        let mut request_options = options();
        request_options.reasoning_effort = Some(ThinkingLevel::Medium);
        request_options.thinking_budgets = Some(ThinkingBudgets {
            medium: Some(4_096.0),
            high: Some(8_192.0),
            ..ThinkingBudgets::default()
        });

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            supports_thinking_token_budget: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        assert_eq!(
            payload(&test_model, &context(), &request_options)["thinking_token_budget"],
            4_096
        );

        test_model.compat = completions_compat(OpenAICompletionsCompat::default());
        assert!(
            payload(&test_model, &context(), &request_options)
                .get("thinking_token_budget")
                .is_none()
        );

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            supports_thinking_token_budget: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        request_options.reasoning_effort = None;
        assert!(
            payload(&test_model, &context(), &request_options)
                .get("thinking_token_budget")
                .is_none()
        );

        for effort in [ThinkingLevel::Xhigh, ThinkingLevel::Max] {
            request_options.reasoning_effort = Some(effort);
            assert_eq!(
                payload(&test_model, &context(), &request_options)["thinking_token_budget"],
                8_192
            );
        }

        request_options.reasoning_effort = Some(ThinkingLevel::High);
        request_options.stream.max_tokens = Some(8_192);
        assert_eq!(
            payload(&test_model, &context(), &request_options)["thinking_token_budget"],
            7_168
        );
        request_options.stream.max_tokens = Some(4_096);
        assert_eq!(
            payload(&test_model, &context(), &request_options)["thinking_token_budget"],
            3_072
        );

        for (field, name) in [
            (ThinkingTokenBudgetField::ThinkingBudget, "thinking_budget"),
            (
                ThinkingTokenBudgetField::ThinkingBudgetTokens,
                "thinking_budget_tokens",
            ),
        ] {
            test_model.compat = completions_compat(OpenAICompletionsCompat {
                thinking_token_budget_field: Some(field),
                ..OpenAICompletionsCompat::default()
            });
            let body = payload(&test_model, &context(), &request_options);
            assert_eq!(body[name], 3_072);
            assert!(body.get("thinking_token_budget").is_none());
        }

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            supports_thinking_token_budget: Some(true),
            thinking_token_budget_field: Some(ThinkingTokenBudgetField::ThinkingBudgetTokens),
            ..OpenAICompletionsCompat::default()
        });
        let body = payload(&test_model, &context(), &request_options);
        assert_eq!(body["thinking_budget_tokens"], 3_072);
        assert!(body.get("thinking_token_budget").is_none());

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::ChatTemplate),
            chat_template_kwargs: Some(BTreeMap::from([(
                "budget".to_owned(),
                ChatTemplateKwargValue::Variable {
                    variable: ThinkingVariable::Budget,
                    omit_when_off: None,
                },
            )])),
            ..OpenAICompletionsCompat::default()
        });
        assert_eq!(
            payload(&test_model, &context(), &request_options)["chat_template_kwargs"],
            json!({"budget":3072})
        );
        request_options.reasoning_effort = None;
        assert!(
            payload(&test_model, &context(), &request_options)["chat_template_kwargs"]
                .get("budget")
                .is_none()
        );
    }

    /// Ports the synchronous replay cases from reasoning-details.test.ts and
    /// thinking-as-text.test.ts, including legacy encrypted signatures.
    #[test]
    fn reasoning_replay_and_thinking_as_text_cases_match_pi() {
        let mut test_model = model("https://example.invalid/v1".to_owned());
        test_model.reasoning = true;
        test_model.provider = "openrouter".into();
        let encrypted = json!({"type":"reasoning.encrypted","id":"call-1","data":"cipher"});
        let signed = json!({"type":"reasoning.text","text":"think","signature":"sig"});
        let mut assistant =
            AssistantMessage::pending("openai-completions", "openrouter", "test-model", 2);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.reasoning_details = Some(vec![signed.clone()]);
        let mut call = ToolCall::new("call-1", "read", Map::new());
        call.thought_signature = Some(serde_json::to_string(&encrypted).unwrap());
        assistant.content = vec![AssistantContent::ToolCall(call)].into();
        let replay_context = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(Box::new(assistant.clone()))],
            tools: None,
        };
        let messages = convert_messages(&test_model, &replay_context).unwrap();
        assert_eq!(messages[0]["reasoning_details"], json!([signed]));

        assistant.reasoning_details = None;
        let replay_context = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(Box::new(assistant))],
            tools: None,
        };
        let messages = convert_messages(&test_model, &replay_context).unwrap();
        assert_eq!(messages[0]["reasoning_details"], json!([encrypted]));

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            requires_thinking_as_text: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        let mut thinking_only =
            AssistantMessage::pending("openai-completions", "openrouter", "test-model", 3);
        thinking_only.stop_reason = StopReason::Stop;
        thinking_only.content =
            vec![AssistantContent::Thinking(ThinkingContent::new("private"))].into();
        let messages = convert_messages(
            &test_model,
            &Context {
                system_prompt: None,
                messages: vec![Message::Assistant(Box::new(thinking_only))],
                tools: None,
            },
        )
        .unwrap();
        assert_eq!(
            messages[0],
            json!({"role":"assistant","content":[{"type":"text","text":"private"}]})
        );
    }

    /// Ports the instruction-role and reasoning-content replay cases from
    /// tool-choice.test.ts using inline catalog-equivalent model fields.
    #[test]
    fn instruction_roles_and_reasoning_replay_fields_match_pi() {
        let mut test_model = model("https://openrouter.ai/api/v1".to_owned());
        test_model.provider = "openrouter".into();
        test_model.reasoning = true;
        test_model.id = "google/gemini".to_owned();
        let messages = convert_messages(&test_model, &context()).unwrap();
        assert_eq!(messages[0]["role"], "system");

        test_model.id = "anthropic/claude-sonnet".to_owned();
        let messages = convert_messages(&test_model, &context()).unwrap();
        assert_eq!(messages[0]["role"], "developer");

        test_model.provider = "openai".into();
        test_model.base_url = "https://api.openai.com/v1".to_owned();
        test_model.id = "gpt-5".to_owned();
        let messages = convert_messages(&test_model, &context()).unwrap();
        assert_eq!(messages[0]["role"], "developer");

        test_model.provider = "opencode-go".into();
        test_model.id = "kimi-k2.6".to_owned();
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            requires_reasoning_content_on_assistant_messages: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        let mut assistant =
            AssistantMessage::pending("openai-completions", "opencode-go", "kimi-k2.6", 2);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![AssistantContent::ToolCall(ToolCall::new(
            "call-1",
            "read",
            Map::new(),
        ))]
        .into();
        let messages = convert_messages(
            &test_model,
            &Context {
                system_prompt: None,
                messages: vec![Message::Assistant(Box::new(assistant.clone()))],
                tools: None,
            },
        )
        .unwrap();
        assert_eq!(messages[0]["reasoning_content"], "");

        let mut thinking = ThinkingContent::new("private");
        thinking.thinking_signature = Some("reasoning".to_owned());
        assistant
            .content
            .insert(0, AssistantContent::Thinking(thinking));
        let messages = convert_messages(
            &test_model,
            &Context {
                system_prompt: None,
                messages: vec![Message::Assistant(Box::new(assistant))],
                tools: None,
            },
        )
        .unwrap();
        assert_eq!(messages[0]["reasoning_content"], "private");
        assert!(messages[0].get("reasoning").is_none());
    }

    /// Pins reasoning-field precedence, OpenCode Go signature normalization,
    /// unknown finish reasons, and choice-level usage from tool-choice.test.ts.
    #[test]
    fn raw_stream_field_semantics_match_pi() {
        let mut test_model = model("https://example.invalid/v1".to_owned());
        test_model.reasoning = true;
        test_model.provider = "opencode-go".into();
        let compat = get_compat(&test_model);
        let (sender, _event_stream) = AssistantMessageEventStream::channel();
        let mut output = pending_message(&test_model);
        let mut state = StreamingState::default();
        process_chunk(
            &sender,
            &test_model,
            &compat,
            &BTreeMap::new(),
            &mut output,
            &mut state,
            RawChunk {
                id: Some("chunk".to_owned()),
                choices: vec![RawChoice {
                    delta: Some(RawDelta {
                        reasoning: Some("private".to_owned()),
                        reasoning_text: Some("duplicate".to_owned()),
                        ..RawDelta::default()
                    }),
                    ..RawChoice::default()
                }],
                ..RawChunk::default()
            },
        )
        .unwrap();
        let AssistantContent::Thinking(thinking) = &output.content[0] else {
            panic!("thinking");
        };
        assert_eq!(thinking.thinking, "private");
        assert_eq!(
            thinking.thinking_signature.as_deref(),
            Some("reasoning_content")
        );

        let (stop_reason, error) = map_stop_reason("unexpected_provider_reason");
        assert_eq!(stop_reason, StopReason::Error);
        assert_eq!(
            error.as_deref(),
            Some("Provider finish_reason: unexpected_provider_reason")
        );

        let usage = parse_chunk_usage(
            &RawUsage {
                prompt_tokens: Some(100.into()),
                completion_tokens: Some(30.into()),
                cached_tokens: Some(20.into()),
                prompt_tokens_details: Some(RawPromptDetails {
                    cached_tokens: Some(10.into()),
                    cache_write_tokens: Some(5.into()),
                }),
                completion_tokens_details: Some(RawCompletionDetails {
                    reasoning_tokens: Some(12.into()),
                }),
                ..RawUsage::default()
            },
            &test_model,
        );
        assert_eq!(usage.input, 85);
        assert_eq!(usage.output, 30);
        assert_eq!(usage.cache_read, 10);
        assert_eq!(usage.cache_write, 5);
        assert_eq!(usage.reasoning, Some(12.into()));
        assert_eq!(usage.total_tokens, 130);
    }

    /// Pins provider/base-URL compatibility detection cases exercised by
    /// empty-tools.test.ts and tool-choice.test.ts inline catalog fixtures.
    #[test]
    fn detected_compatibility_families_match_pi() {
        let cases = [
            (
                "cloudflare-ai-gateway",
                "https://gateway.ai.cloudflare.com/v1/a/g/compat",
                MaxTokensField::MaxTokens,
                ThinkingFormat::Openai,
                false,
                false,
            ),
            (
                "deepseek",
                "https://api.deepseek.com",
                MaxTokensField::MaxTokens,
                ThinkingFormat::Deepseek,
                true,
                false,
            ),
            (
                "zai",
                "https://api.z.ai/api/paas/v4",
                MaxTokensField::MaxTokens,
                ThinkingFormat::Zai,
                false,
                false,
            ),
            (
                "together",
                "https://api.together.xyz/v1",
                MaxTokensField::MaxTokens,
                ThinkingFormat::Together,
                false,
                false,
            ),
            (
                "ant-ling",
                "https://api.ant-ling.com/v1",
                MaxTokensField::MaxTokens,
                ThinkingFormat::AntLing,
                false,
                false,
            ),
            (
                "openrouter",
                "https://openrouter.ai/api/v1",
                MaxTokensField::MaxCompletionTokens,
                ThinkingFormat::Openrouter,
                true,
                true,
            ),
        ];
        for (provider, base_url, max_field, format, supports_effort, supports_store) in cases {
            let mut test_model = model(base_url.to_owned());
            test_model.provider = provider.into();
            let compat = detect_compat(&test_model);
            assert_eq!(compat.max_tokens_field, max_field, "{provider}");
            assert_eq!(compat.thinking_format, format, "{provider}");
            assert_eq!(
                compat.supports_reasoning_effort, supports_effort,
                "{provider}"
            );
            assert_eq!(compat.supports_store, supports_store, "{provider}");
            let mut request_options = options();
            request_options.stream.max_tokens = Some(1_234);
            let body = payload(&test_model, &context(), &request_options);
            match max_field {
                MaxTokensField::MaxTokens => {
                    assert_eq!(body["max_tokens"], 1_234, "{provider}");
                    assert!(body.get("max_completion_tokens").is_none(), "{provider}");
                }
                MaxTokensField::MaxCompletionTokens => {
                    assert_eq!(body["max_completion_tokens"], 1_234, "{provider}");
                    assert!(body.get("max_tokens").is_none(), "{provider}");
                }
            }
        }

        let mut open_router = model("https://openrouter.ai/api/v1".to_owned());
        open_router.provider = "openrouter".into();
        open_router.id = "anthropic/claude-sonnet".to_owned();
        let compat = detect_compat(&open_router);
        assert!(compat.supports_developer_role);
        assert_eq!(
            compat.cache_control_format,
            Some(CacheControlFormat::Anthropic)
        );
        open_router.id = "google/gemini".to_owned();
        assert!(!detect_compat(&open_router).supports_developer_role);

        let mut grok = model("https://api.x.ai/v1".to_owned());
        grok.provider = "xai".into();
        grok.reasoning = true;
        let mut request_options = options();
        request_options.reasoning_effort = Some(ThinkingLevel::High);
        assert!(
            payload(&grok, &context(), &request_options)
                .get("reasoning_effort")
                .is_none()
        );
    }

    /// Ports the prompt-cache payload cases from
    /// openai-completions-prompt-cache.test.ts without transport setup.
    #[test]
    fn prompt_cache_payload_matrix_matches_pi() {
        let direct = model("https://api.openai.com/v1".to_owned());
        let mut request_options = options();
        request_options.stream.session_id = Some("session-key".to_owned());
        let body = payload(&direct, &context(), &request_options);
        assert_eq!(body["prompt_cache_key"], "session-key");
        assert!(body.get("prompt_cache_retention").is_none());

        request_options.stream.cache_retention = Some(CacheRetention::Long);
        let body = payload(&direct, &context(), &request_options);
        assert_eq!(body["prompt_cache_retention"], "24h");
        request_options.stream.session_id = Some("x".repeat(65));
        assert_eq!(
            payload(&direct, &context(), &request_options)["prompt_cache_key"],
            "x".repeat(64)
        );

        request_options.stream.cache_retention = Some(CacheRetention::None);
        let body = payload(&direct, &context(), &request_options);
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());

        let mut compatible = model("https://example.invalid/v1".to_owned());
        request_options.stream.cache_retention = Some(CacheRetention::Short);
        assert!(
            payload(&compatible, &context(), &request_options)
                .get("prompt_cache_key")
                .is_none()
        );
        request_options.stream.cache_retention = Some(CacheRetention::Long);
        let body = payload(&compatible, &context(), &request_options);
        assert_eq!(body["prompt_cache_key"], "x".repeat(64));
        assert_eq!(body["prompt_cache_retention"], "24h");

        compatible.compat = completions_compat(OpenAICompletionsCompat {
            supports_long_cache_retention: Some(false),
            ..OpenAICompletionsCompat::default()
        });
        let body = payload(&compatible, &context(), &request_options);
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());

        request_options.stream.cache_retention = None;
        request_options.stream.request.env = Some(ProviderEnv::from([(
            "PI_CACHE_RETENTION".to_owned(),
            "long".to_owned(),
        )]));
        assert_eq!(
            payload(&direct, &context(), &request_options)["prompt_cache_retention"],
            "24h"
        );
    }

    /// Ports openai-completions-empty-tools.test.ts cases for absent and empty
    /// tools, default and explicit max tokens, context clamping, and tool history.
    #[tokio::test]
    async fn empty_tools_and_max_token_cases_match_pi() {
        for tools in [None, Some(Vec::new())] {
            let server =
                start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
            let mut test_context = context();
            test_context.tools = tools;
            let (_, captured) = run_and_capture(
                model(server.base_url.clone()),
                test_context,
                options(),
                &server,
            )
            .await;
            assert!(captured.body.get("tools").is_none());
        }

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let test_model = model(server.base_url.clone());
        let mut simple = SimpleStreamOptions::default();
        simple.stream.request.api_key = Some("test-key".to_owned());
        let mut event_stream = stream_simple(&test_model, &context(), simple);
        while event_stream.next().await.is_some() {}
        assert_eq!(
            server.captured.lock().unwrap()[0].body["max_completion_tokens"],
            4_096
        );

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let test_model = model(server.base_url.clone());
        let mut simple = SimpleStreamOptions::default();
        simple.stream.request.api_key = Some("test-key".to_owned());
        simple.stream.max_tokens = Some(1_234);
        let mut event_stream = stream_simple(&test_model, &context(), simple);
        while event_stream.next().await.is_some() {}
        assert_eq!(
            server.captured.lock().unwrap()[0].body["max_completion_tokens"],
            1_234
        );

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut constrained_model = model(server.base_url.clone());
        constrained_model.context_window = 10_000;
        constrained_model.max_tokens = 8_000;
        let long_context = Context {
            system_prompt: None,
            messages: vec![Message::User(Box::new(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("x".repeat(8_000)),
                timestamp: 1,
            }))],
            tools: None,
        };
        let mut simple = SimpleStreamOptions::default();
        simple.stream.request.api_key = Some("test-key".to_owned());
        let mut event_stream = stream_simple(&constrained_model, &long_context, simple);
        while event_stream.next().await.is_some() {}
        assert_eq!(
            server.captured.lock().unwrap()[0].body["max_completion_tokens"],
            3_904
        );

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        constrained_model.base_url.clone_from(&server.base_url);
        let mut simple = SimpleStreamOptions::default();
        simple.stream.request.api_key = Some("test-key".to_owned());
        simple.stream.max_tokens = Some(7_000);
        let mut event_stream = stream_simple(&constrained_model, &long_context, simple);
        while event_stream.next().await.is_some() {}
        assert_eq!(
            server.captured.lock().unwrap()[0].body["max_completion_tokens"],
            3_904
        );

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut assistant =
            AssistantMessage::pending("openai-completions", "openai", "test-model", 2);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![AssistantContent::ToolCall(ToolCall::new(
            "t1",
            "noop",
            Map::new(),
        ))]
        .into();
        let tool_history = Context {
            system_prompt: None,
            messages: vec![
                context().messages[0].clone(),
                Message::Assistant(Box::new(assistant)),
            ],
            tools: None,
        };
        let mut simple = SimpleStreamOptions::default();
        simple.stream.request.api_key = Some("test-key".to_owned());
        let mut event_stream =
            stream_simple(&model(server.base_url.clone()), &tool_history, simple);
        while event_stream.next().await.is_some() {}
        assert_eq!(server.captured.lock().unwrap()[0].body["tools"], json!([]));
    }

    /// Ports openai-completions-prompt-cache.test.ts payload and header cases.
    #[tokio::test]
    async fn prompt_cache_and_session_affinity_cases_match_pi() {
        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], true).await;
        let mut request_options = options();
        request_options.stream.session_id = Some("x".repeat(67));
        request_options.stream.cache_retention = Some(CacheRetention::Long);
        let (_, captured) = run_and_capture(
            model(server.base_url.clone()),
            context(),
            request_options,
            &server,
        )
        .await;
        assert_eq!(captured.body["prompt_cache_key"], "x".repeat(64));
        assert_eq!(captured.body["prompt_cache_retention"], "24h");

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            send_session_affinity_headers: Some(true),
            session_affinity_format: Some(SessionAffinityFormat::OpenaiNosession),
            ..OpenAICompletionsCompat::default()
        });
        let mut request_options = options();
        request_options.stream.session_id = Some("session-nosession".to_owned());
        let (_, captured) = run_and_capture(test_model, context(), request_options, &server).await;
        assert!(!captured.headers.contains_key("session_id"));
        assert_eq!(
            captured
                .headers
                .get("x-client-request-id")
                .map(String::as_str),
            Some("session-nosession")
        );
        assert_eq!(
            captured
                .headers
                .get("x-session-affinity")
                .map(String::as_str),
            Some("session-nosession")
        );

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.provider = "openrouter".into();
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            send_session_affinity_headers: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        let mut request_options = options();
        request_options.stream.session_id = Some("router-session".to_owned());
        let (_, captured) = run_and_capture(test_model, context(), request_options, &server).await;
        assert_eq!(
            captured.headers.get("x-session-id").map(String::as_str),
            Some("router-session")
        );

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            send_session_affinity_headers: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        let mut request_options = options();
        request_options.stream.session_id = Some("openai-session".to_owned());
        let (_, captured) =
            run_and_capture(test_model.clone(), context(), request_options, &server).await;
        assert_eq!(
            captured.headers.get("session_id").map(String::as_str),
            Some("openai-session")
        );
        assert_eq!(
            captured
                .headers
                .get("x-client-request-id")
                .map(String::as_str),
            Some("openai-session")
        );
        assert_eq!(
            captured
                .headers
                .get("x-session-affinity")
                .map(String::as_str),
            Some("openai-session")
        );

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        test_model.base_url.clone_from(&server.base_url);
        let mut request_options = options();
        request_options.stream.session_id = Some("uncached".to_owned());
        request_options.stream.cache_retention = Some(CacheRetention::None);
        let (_, captured) =
            run_and_capture(test_model.clone(), context(), request_options, &server).await;
        assert!(!captured.headers.contains_key("session_id"));
        assert!(!captured.headers.contains_key("x-client-request-id"));
        assert!(!captured.headers.contains_key("x-session-affinity"));

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        test_model.base_url.clone_from(&server.base_url);
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            send_session_affinity_headers: Some(false),
            ..OpenAICompletionsCompat::default()
        });
        let mut request_options = options();
        request_options.stream.session_id = Some("disabled".to_owned());
        let (_, captured) =
            run_and_capture(test_model.clone(), context(), request_options, &server).await;
        assert!(!captured.headers.contains_key("session_id"));
        assert!(!captured.headers.contains_key("x-client-request-id"));
        assert!(!captured.headers.contains_key("x-session-affinity"));

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            send_session_affinity_headers: Some(true),
            ..OpenAICompletionsCompat::default()
        });

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        test_model.base_url.clone_from(&server.base_url);
        let mut request_options = options();
        request_options.stream.session_id = Some("generated".to_owned());
        request_options.stream.request.headers = Some(ProviderHeaders::from([
            ("session_id".to_owned(), Some("explicit-session".to_owned())),
            (
                "x-client-request-id".to_owned(),
                Some("explicit-request".to_owned()),
            ),
            (
                "x-session-affinity".to_owned(),
                Some("explicit-affinity".to_owned()),
            ),
        ]));
        let (_, captured) = run_and_capture(test_model, context(), request_options, &server).await;
        assert_eq!(captured.headers["session_id"], "explicit-session");
        assert_eq!(captured.headers["x-client-request-id"], "explicit-request");
        assert_eq!(captured.headers["x-session-affinity"], "explicit-affinity");
    }

    /// Ports all four openai-completions-cache-control-format.test.ts cases.
    #[tokio::test]
    async fn anthropic_cache_control_markers_match_pi() {
        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.provider = "openrouter".into();
        test_model.reasoning = true;
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            cache_control_format: Some(CacheControlFormat::Anthropic),
            ..OpenAICompletionsCompat::default()
        });
        let mut test_context = context();
        test_context.tools = Some(vec![Tool {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            constrained_sampling: None,
        }]);
        let (_, captured) =
            run_and_capture(test_model.clone(), test_context.clone(), options(), &server).await;
        assert_eq!(
            captured.body["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            captured.body["tools"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        let last = captured.body["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(
            last["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );

        let mut detected_model = test_model.clone();
        detected_model.id = "anthropic/claude-sonnet-4".to_owned();
        detected_model.compat = None;
        let detected = payload(&detected_model, &test_context, &options());
        assert_eq!(
            detected["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );

        let mut assistant = AssistantMessage::pending(
            "openai-completions",
            "openrouter",
            "anthropic/claude-sonnet-4",
            2,
        );
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![AssistantContent::ToolCall(ToolCall::new(
            "call_1",
            "read",
            Map::from_iter([("path".to_owned(), Value::String("README.md".to_owned()))]),
        ))]
        .into();
        let tool_context = Context {
            system_prompt: Some("System prompt".to_owned()),
            messages: vec![
                context().messages[0].clone(),
                Message::Assistant(Box::new(assistant)),
                Message::ToolResult(Box::new(ToolResultMessage {
                    role: ToolResultRole::ToolResult,
                    tool_call_id: "call_1".to_owned(),
                    tool_name: "read".to_owned(),
                    content: vec![crate::types::UserContentBlock::Text(TextContent::new(
                        "file contents",
                    ))],
                    details: None,
                    usage: None,
                    added_tool_names: None,
                    is_error: false,
                    timestamp: 3,
                })),
            ],
            tools: test_context.tools.clone(),
        };
        let tool_payload = payload(&detected_model, &tool_context, &options());
        let user_message = tool_payload["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "user")
            .unwrap();
        assert!(user_message["content"].is_string());
        let tool_message = tool_payload["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(tool_message["role"], "tool");
        assert_eq!(
            tool_message["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        test_model.base_url.clone_from(&server.base_url);
        let mut no_cache = options();
        no_cache.stream.cache_retention = Some(CacheRetention::None);
        let (_, captured) = run_and_capture(test_model, test_context, no_cache, &server).await;
        assert!(captured.body["messages"][0]["content"].is_string());
        assert!(captured.body["tools"][0].get("cache_control").is_none());
    }

    /// Ports raw-stop-reason and response-model test files case-for-case.
    #[tokio::test]
    async fn raw_stop_reason_response_id_and_response_model_match_pi() {
        let chunks = vec![
            json!({
                "id":"chatcmpl-route",
                "model":"routed/model",
                "choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]
            }),
            json!({
                "id":"chatcmpl-route",
                "model":"routed/model",
                "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]
            }),
        ];
        let server = start_server(vec![Script::sse(chunks)], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.id = "router/auto".to_owned();
        test_model.provider = "openrouter".into();
        let (message, _) = run_and_capture(test_model, context(), options(), &server).await;
        assert_eq!(message.model, "router/auto");
        assert_eq!(message.response_model.as_deref(), Some("routed/model"));
        assert_eq!(message.response_id.as_deref(), Some("chatcmpl-route"));
        assert_eq!(message.raw_stop_reason.as_deref(), Some("stop"));
        assert_eq!(message.stop_reason, StopReason::Stop);

        let server = start_server(
            vec![Script::sse(vec![json!({
                "id":"chatcmpl-filter",
                "choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]
            })])],
            false,
        )
        .await;
        let (message, _) = run_and_capture(
            model(server.base_url.clone()),
            context(),
            options(),
            &server,
        )
        .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.raw_stop_reason.as_deref(), Some("content_filter"));
        assert_eq!(
            message.error_message.as_deref(),
            Some("Provider finish_reason: content_filter")
        );
    }

    /// Pins pi OpenAI SDK `core/streaming.js:49-50` and
    /// `src/api/openai-completions.ts:664-683`: any truthy top-level stream
    /// error is terminal, including the OpenRouter metadata.raw appendix.
    #[tokio::test]
    async fn truthy_top_level_error_chunk_is_terminal_with_raw_metadata() {
        let chunk = json!({
            "error": {
                "message": "upstream stream failure",
                "metadata": {"raw": "OpenRouter upstream detail"}
            }
        });
        let mut test_model = model("https://example.invalid/v1".to_owned());
        test_model.provider = "openrouter".into();
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            supports_finish_reason: Some(false),
            ..OpenAICompletionsCompat::default()
        });
        let mut request_options = options();
        request_options.stream.request.fetch = Some(Arc::new(StaticFetch(
            format!("data: {chunk}\n\ndata: [DONE]\n\n").into_bytes(),
        )));

        let mut event_stream = stream(&test_model, &context(), request_options);
        let mut events = Vec::new();
        while let Some(event) = event_stream.next().await {
            events.push(event);
        }
        let message = event_stream.result().await.unwrap();

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AssistantMessageEvent::Start));
        assert!(matches!(events[1], AssistantMessageEvent::Error { .. }));
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("upstream stream failure\nOpenRouter upstream detail")
        );
    }

    /// Pins pi OpenAI SDK `core/streaming.js:41-50,255-267` and
    /// `src/api/openai-completions.ts:664-683`: an event-only SSE message
    /// reaches JSON.parse as empty data and becomes an in-band terminal error.
    #[tokio::test]
    async fn event_only_sse_message_is_a_terminal_json_parse_error() {
        let mut request_options = options();
        request_options.stream.request.fetch = Some(Arc::new(StaticFetch(
            b"event: future\n\ndata: [DONE]\n\n".to_vec(),
        )));
        let mut event_stream = stream(
            &model("https://example.invalid/v1".to_owned()),
            &context(),
            request_options,
        );
        let mut events = Vec::new();
        while let Some(event) = event_stream.next().await {
            events.push(event);
        }
        let message = event_stream.result().await.unwrap();

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AssistantMessageEvent::Start));
        assert!(matches!(events[1], AssistantMessageEvent::Error { .. }));
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("Unexpected end of JSON input")
        );
    }

    /// Ports response-model empty/echo cases and null-chunk/finish-reason behavior
    /// from response-model and tool-choice tests.
    #[tokio::test]
    async fn chunk_edge_cases_match_pi() {
        let server = start_server(
            vec![Script::sse(vec![
                Value::Null,
                json!({
                    "id":"chatcmpl-edge",
                    "model":"test-model",
                    "choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]
                }),
                stop_chunk("test-model"),
            ])],
            false,
        )
        .await;
        let (message, _) = run_and_capture(
            model(server.base_url.clone()),
            context(),
            options(),
            &server,
        )
        .await;
        assert!(message.response_model.is_none());
        assert_eq!(message.stop_reason, StopReason::Stop);

        let server = start_server(
            vec![Script::sse(vec![json!({
                "id":"chatcmpl-no-finish",
                "choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]
            })])],
            false,
        )
        .await;
        let (message, _) = run_and_capture(
            model(server.base_url.clone()),
            context(),
            options(),
            &server,
        )
        .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("Stream ended without finish_reason")
        );

        // pi `openai-completions.ts:527` ignores a falsy finish reason.
        let server = start_server(
            vec![Script::sse(vec![json!({
                "id":"chatcmpl-empty-finish",
                "choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":""}]
            })])],
            false,
        )
        .await;
        let (message, _) = run_and_capture(
            model(server.base_url.clone()),
            context(),
            options(),
            &server,
        )
        .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(message.raw_stop_reason.is_none());
        assert_eq!(
            message.error_message.as_deref(),
            Some("Stream ended without finish_reason")
        );

        let server = start_server(
            vec![Script::sse(vec![json!({
                "id":"chatcmpl-no-finish",
                "choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]
            })])],
            false,
        )
        .await;
        let mut test_model = model(server.base_url.clone());
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            supports_finish_reason: Some(false),
            ..OpenAICompletionsCompat::default()
        });
        let (message, _) = run_and_capture(test_model, context(), options(), &server).await;
        assert_eq!(message.stop_reason, StopReason::Stop);
    }

    /// Ports reasoning-details.test.ts streaming, replay, fallback, and ordering.
    #[tokio::test]
    async fn reasoning_details_stream_and_replay_match_pi() {
        let encrypted = json!({"type":"reasoning.encrypted","id":"call_1","data":"ciphertext"});
        let signed = json!({
            "type":"reasoning.text","text":"think","signature":"sig",
            "id":"detail-1","format":"claude","index":0
        });
        let summary = json!({
            "type":"reasoning.summary","summary":"summary",
            "id":"detail-2","format":"claude","index":1
        });
        let chunks = vec![
            json!({"id":"c","choices":[{"index":0,"delta":{"reasoning_details":[signed.clone(), encrypted.clone(), summary.clone()]},"finish_reason":null}]}),
            json!({"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"README.md\"}"}}]},"finish_reason":null}]}),
            json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let server = start_server(vec![Script::sse(chunks)], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.reasoning = true;
        test_model.provider = "openrouter".into();
        let mut test_context = context();
        test_context.tools = Some(vec![Tool {
            name: "read".to_owned(),
            description: "Read".to_owned(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            constrained_sampling: None,
        }]);
        let (message, _) = run_and_capture(test_model, test_context, options(), &server).await;
        assert_eq!(
            message.reasoning_details,
            Some(vec![signed, encrypted.clone(), summary])
        );
        let AssistantContent::ToolCall(call) = &message.content[0] else {
            panic!("expected tool call");
        };
        assert_eq!(
            call.thought_signature.as_deref(),
            Some(serde_json::to_string(&encrypted).unwrap().as_str())
        );
        assert_eq!(call.arguments, json!({"path":"README.md"}));
    }

    /// Ports thinking-as-text.test.ts conversion and real SSE endpoint cases.
    #[tokio::test]
    async fn thinking_as_text_replay_matches_pi() {
        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.reasoning = true;
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            requires_thinking_as_text: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        let mut assistant =
            AssistantMessage::pending("openai-completions", "openai", "test-model", 2);
        assistant.stop_reason = StopReason::Stop;
        assistant.content = vec![
            AssistantContent::Thinking(ThinkingContent::new("internal reasoning")),
            AssistantContent::Text(TextContent::new("visible answer")),
        ]
        .into();
        let test_context = Context {
            system_prompt: None,
            messages: vec![
                context().messages[0].clone(),
                Message::Assistant(Box::new(assistant)),
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text("continue".to_owned()),
                    timestamp: 3,
                })),
            ],
            tools: None,
        };
        let (_, captured) = run_and_capture(test_model, test_context, options(), &server).await;
        assert_eq!(
            captured.body["messages"][1],
            json!({
                "role":"assistant",
                "content":[
                    {"type":"text","text":"internal reasoning"},
                    {"type":"text","text":"visible answer"}
                ]
            })
        );
    }

    /// Ports all thinking-token-budget.test.ts cases, including field presence,
    /// clamping, aliases, and chat-template variables.
    #[tokio::test]
    async fn thinking_token_budget_cases_match_pi() {
        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.reasoning = true;
        test_model.max_tokens = 16_384;
        test_model.provider = "local-vllm".into();
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::Zai),
            supports_thinking_token_budget: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        let mut simple = SimpleStreamOptions::default();
        simple.stream.request.api_key = Some("test".to_owned());
        simple.reasoning = Some(ThinkingLevel::Medium);
        simple.thinking_budgets = Some(ThinkingBudgets {
            medium: Some(4_096.0),
            ..ThinkingBudgets::default()
        });
        let mut event_stream = stream_simple(&test_model, &context(), simple);
        while event_stream.next().await.is_some() {}
        assert_eq!(
            server.captured.lock().unwrap()[0].body["thinking_token_budget"],
            4_096
        );

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        test_model.base_url.clone_from(&server.base_url);
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::Qwen),
            thinking_token_budget_field: Some(ThinkingTokenBudgetField::ThinkingBudgetTokens),
            supports_thinking_token_budget: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        let mut simple = SimpleStreamOptions::default();
        simple.stream.request.api_key = Some("test".to_owned());
        simple.reasoning = Some(ThinkingLevel::High);
        simple.stream.max_tokens = Some(4_096);
        let mut event_stream = stream_simple(&test_model, &context(), simple);
        while event_stream.next().await.is_some() {}
        let body = server.captured.lock().unwrap()[0].body.clone();
        assert_eq!(body["thinking_budget_tokens"], 3_072);
        assert!(body.get("thinking_token_budget").is_none());

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        test_model.base_url.clone_from(&server.base_url);
        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::ChatTemplate),
            chat_template_kwargs: Some(BTreeMap::from([
                (
                    "enable_thinking".to_owned(),
                    ChatTemplateKwargValue::Variable {
                        variable: ThinkingVariable::Enabled,
                        omit_when_off: None,
                    },
                ),
                (
                    "thinking_budget".to_owned(),
                    ChatTemplateKwargValue::Variable {
                        variable: ThinkingVariable::Budget,
                        omit_when_off: None,
                    },
                ),
            ])),
            ..OpenAICompletionsCompat::default()
        });
        let mut simple = SimpleStreamOptions::default();
        simple.stream.request.api_key = Some("test".to_owned());
        let mut event_stream = stream_simple(&test_model, &context(), simple);
        while event_stream.next().await.is_some() {}
        assert_eq!(
            server.captured.lock().unwrap()[0].body["chat_template_kwargs"],
            json!({"enable_thinking":false})
        );
    }

    /// Ports tool-result-images.test.ts image batching and empty placeholder cases.
    #[test]
    fn tool_result_image_conversion_matches_pi() {
        let mut test_model = model("http://127.0.0.1:1".to_owned());
        test_model.input.push(ModelInput::Image);
        let mut assistant =
            AssistantMessage::pending("openai-completions", "openai", "test-model", 2);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![
            AssistantContent::ToolCall(ToolCall::new("tool-1", "read", Map::new())),
            AssistantContent::ToolCall(ToolCall::new("tool-2", "read", Map::new())),
        ]
        .into();
        let tool_result = |id: &str, image: bool| {
            let mut content = vec![crate::types::UserContentBlock::Text(TextContent::new(
                if image { "read image" } else { "" },
            ))];
            if image {
                content.push(crate::types::UserContentBlock::Image(ImageContent::new(
                    "ZmFrZQ==",
                    "image/png",
                )));
            }
            Message::ToolResult(Box::new(ToolResultMessage {
                role: ToolResultRole::ToolResult,
                tool_call_id: id.to_owned(),
                tool_name: "read".to_owned(),
                content,
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: 3,
            }))
        };
        let test_context = Context {
            system_prompt: None,
            messages: vec![
                context().messages[0].clone(),
                Message::Assistant(Box::new(assistant)),
                tool_result("tool-1", true),
                tool_result("tool-2", true),
            ],
            tools: None,
        };
        let messages = convert_messages(&test_model, &test_context).unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["user", "assistant", "tool", "tool", "user"]
        );
        assert_eq!(
            messages.last().unwrap()["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|part| part["type"] == "image_url")
                .count(),
            2
        );

        let mut empty_context = test_context;
        empty_context.messages.truncate(2);
        empty_context.messages.push(tool_result("tool-1", false));
        let messages = convert_messages(&test_model, &empty_context).unwrap();
        assert_eq!(
            messages
                .iter()
                .find(|message| message["role"] == "tool")
                .unwrap()["content"],
            "(no tool output)"
        );
    }

    /// Ports usage cases from tool-choice.test.ts, including choice-level Kimi
    /// fallback and no double-counting of reasoning tokens.
    #[tokio::test]
    async fn usage_and_choice_fallback_match_pi() {
        let usage = json!({
            "prompt_tokens":100,
            "completion_tokens":30,
            "prompt_tokens_details":{"cached_tokens":20,"cache_write_tokens":10},
            "completion_tokens_details":{"reasoning_tokens":12}
        });
        let server = start_server(
            vec![Script::sse(vec![json!({
                "id":"usage",
                "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                "usage":usage
            })])],
            false,
        )
        .await;
        let mut test_model = model(server.base_url.clone());
        test_model.reasoning = true;
        let (message, _) = run_and_capture(test_model, context(), options(), &server).await;
        assert_eq!(message.usage.input, 70);
        assert_eq!(message.usage.output, 30);
        assert_eq!(message.usage.cache_read, 20);
        assert_eq!(message.usage.cache_write, 10);
        assert_eq!(message.usage.reasoning, Some(12.into()));
        assert_eq!(message.usage.total_tokens, 130);

        let server = start_server(
            vec![Script::sse(vec![json!({
                "id":"usage-choice",
                "choices":[{
                    "index":0,"delta":{},"finish_reason":"stop",
                    "usage":{"prompt_tokens":10,"completion_tokens":2,"cached_tokens":4}
                }]
            })])],
            false,
        )
        .await;
        let mut test_model = model(server.base_url.clone());
        test_model.provider = "moonshotai".into();
        let (message, _) = run_and_capture(test_model, context(), options(), &server).await;
        assert_eq!(message.usage.input, 6);
        assert_eq!(message.usage.cache_read, 4);
        assert_eq!(message.usage.total_tokens, 12);
    }

    /// Ports tool-choice.test.ts payload compatibility families using inline
    /// model fixtures for the exact catalog fields each source case relies on.
    #[tokio::test]
    async fn tool_choice_and_thinking_format_payloads_match_pi() {
        let cases = [
            (
                ThinkingFormat::Zai,
                json!({"thinking":{"type":"enabled","clear_thinking":false}}),
            ),
            (ThinkingFormat::Qwen, json!({"enable_thinking":true})),
            (
                ThinkingFormat::QwenChatTemplate,
                json!({"chat_template_kwargs":{"enable_thinking":true,"preserve_thinking":true}}),
            ),
            (
                ThinkingFormat::Together,
                json!({"reasoning":{"enabled":true}}),
            ),
        ];
        for (format, expected) in cases {
            let server =
                start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
            let mut test_model = model(server.base_url.clone());
            test_model.reasoning = true;
            test_model.provider = "custom".into();
            test_model.compat = completions_compat(OpenAICompletionsCompat {
                thinking_format: Some(format),
                supports_reasoning_effort: Some(false),
                ..OpenAICompletionsCompat::default()
            });
            let mut simple = SimpleStreamOptions::default();
            simple.stream.request.api_key = Some("test".to_owned());
            simple.reasoning = Some(ThinkingLevel::High);
            simple.tool_choice = Some(ToolChoice::Auto);
            let mut event_stream = stream_simple(&test_model, &context(), simple);
            while event_stream.next().await.is_some() {}
            let body = &server.captured.lock().unwrap()[0].body;
            assert_eq!(body["tool_choice"], "auto");
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(&body[key], value);
            }
        }

        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.reasoning = true;
        test_model.provider = "openrouter".into();
        let mut simple = SimpleStreamOptions::default();
        simple.stream.request.api_key = Some("test".to_owned());
        simple.reasoning = Some(ThinkingLevel::High);
        let mut event_stream = stream_simple(&test_model, &context(), simple);
        while event_stream.next().await.is_some() {}
        let body = &server.captured.lock().unwrap()[0].body;
        assert_eq!(body["reasoning"], json!({"effort":"high"}));
        assert!(body.get("reasoning_effort").is_none());
    }

    /// Ports tool-choice.test.ts:661-708 and asserts the partial-free event
    /// sequence so an empty custom object cannot leave custom-mode scratch state.
    #[tokio::test]
    async fn empty_custom_object_on_function_delta_stays_in_function_mode() {
        let chunks = vec![json!({
            "id":"chatcmpl-empty-custom",
            "choices":[{"index":0,"delta":{"tool_calls":[{
                "index":0,
                "id":"call_1",
                "type":"function",
                "function":{"name":"read","arguments":"{\"path\":\"README.md\"}"},
                "custom":{}
            }]},"finish_reason":"tool_calls"}]
        })];
        let server = start_server(vec![Script::sse(chunks)], false).await;
        let test_model = model(server.base_url.clone());
        let mut event_stream = stream(&test_model, &context(), options());
        let mut events = Vec::new();
        while let Some(event) = event_stream.next().await {
            events.push(event);
        }
        let message = event_stream.result().await.unwrap();

        assert_eq!(events.len(), 5);
        assert!(matches!(events[0], AssistantMessageEvent::Start));
        assert!(matches!(
            &events[1],
            AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                id,
                tool_name,
                ..
            } if id == "call_1" && tool_name == "read"
        ));
        assert!(matches!(
            &events[2],
            AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta,
            } if delta == "{\"path\":\"README.md\"}"
        ));
        assert!(matches!(
            &events[3],
            AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call,
            } if tool_call.arguments == json!({"path":"README.md"})
        ));
        assert!(matches!(
            events[4],
            AssistantMessageEvent::Done {
                reason: SuccessfulStopReason::ToolUse,
                ..
            }
        ));
        assert_eq!(message.content.len(), 1);
    }

    /// Pins genuine custom-input mode from openai-completions.ts:455-457 and
    /// its close delta from openai-completions.ts:390-401.
    #[tokio::test]
    async fn genuine_custom_tool_delta_still_uses_custom_input_mode() {
        let chunks = vec![json!({
            "id":"chatcmpl-custom",
            "choices":[{"index":0,"delta":{"tool_calls":[{
                "index":0,
                "id":"call_1",
                "type":"custom",
                "custom":{"name":"sample_tool","input":"abc"}
            }]},"finish_reason":"tool_calls"}]
        })];
        let server = start_server(vec![Script::sse(chunks)], false).await;
        let test_model = model(server.base_url.clone());
        let mut event_stream = stream(&test_model, &context(), options());
        let mut events = Vec::new();
        while let Some(event) = event_stream.next().await {
            events.push(event);
        }
        let message = event_stream.result().await.unwrap();

        let deltas = events
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::ToolCallDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(
            serde_json::from_str::<Value>(&deltas).unwrap(),
            json!({"input":"abc"})
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::ToolCallDelta { .. }))
                .count(),
            2
        );
        let AssistantContent::ToolCall(call) = &message.content[0] else {
            panic!("tool call");
        };
        assert_eq!(call.arguments, json!({"input":"abc"}));
    }

    /// Pins pi `src/api/openai-completions.ts:423-431` start-time thinking signature state.
    #[tokio::test]
    async fn thinking_start_carries_the_signature_already_present_in_pi_partial() {
        let (sender, mut events) = AssistantMessageEventStream::channel();
        let mut output = AssistantMessage::pending("openai-completions", "openai", "model", 1);
        let mut state = StreamingState::default();
        ensure_thinking_block(&sender, &mut output, &mut state, "reasoning_content").unwrap();
        assert!(matches!(
            events.next().await,
            Some(AssistantMessageEvent::ThinkingStart {
                thinking_signature: Some(signature),
                ..
            }) if signature == "reasoning_content"
        ));
    }

    /// Ports the mixed text/reasoning/parallel-tool accumulation and mutable-id
    /// cases from tool-choice.test.ts through openai-oxide raw SSE parsing.
    #[tokio::test]
    async fn mixed_parallel_stream_accumulation_matches_pi() {
        let chunks = vec![
            json!({"id":"mix","choices":[{"index":0,"delta":{
                "reasoning_content":"think ",
                "content":"answer ",
                "tool_calls":[
                    {"index":0,"id":"old-id","function":{"name":"one","arguments":"{\"a\":"}},
                    {"index":1,"id":"two-id","function":{"name":"two","arguments":"{\"b\":"}}
                ]
            },"finish_reason":null}]}),
            json!({"id":"mix","choices":[{"index":0,"delta":{
                "reasoning_content":"more",
                "content":"done",
                "tool_calls":[
                    {"index":0,"id":"new-id","function":{"arguments":"1}"}},
                    {"index":1,"function":{"arguments":"2}"}}
                ]
            },"finish_reason":null}]}),
            json!({"id":"mix","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let server = start_server(vec![Script::sse(chunks)], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.reasoning = true;
        let (message, _) = run_and_capture(test_model, context(), options(), &server).await;
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        let AssistantContent::Text(text) = &message.content[0] else {
            panic!("text");
        };
        let AssistantContent::Thinking(thinking) = &message.content[1] else {
            panic!("thinking");
        };
        let AssistantContent::ToolCall(first) = &message.content[2] else {
            panic!("first tool");
        };
        let AssistantContent::ToolCall(second) = &message.content[3] else {
            panic!("second tool");
        };
        assert_eq!(thinking.thinking, "think more");
        assert_eq!(text.text, "answer done");
        assert_eq!(first.id, "old-id");
        assert_eq!(first.arguments, json!({"a":1}));
        assert_eq!(second.arguments, json!({"b":2}));
    }

    /// Pins pi `src/api/openai-completions.ts:291-318,664-683` and
    /// `src/utils/provider-retry.ts:69-71,113-118`: pre-abort still reaches
    /// payload construction, but the request point emits `Request aborted`.
    #[tokio::test]
    async fn preaborted_signal_runs_payload_without_sending_and_preserves_key_precedence() {
        let signal = Arc::new(ManualAbort::default());
        signal.abort();
        let fetch = Arc::new(RecordingFetch::default());
        let payload_calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = payload_calls.clone();
        let mut request_options = options();
        request_options.stream.request.signal = Some(signal.clone());
        request_options.stream.request.fetch = Some(fetch.clone());
        request_options.stream.request.on_payload = Some(Arc::new(move |_, _| {
            callback_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(None) })
        }));

        let message = run_message(
            &model("https://example.invalid/v1".to_owned()),
            &context(),
            request_options,
        )
        .await;
        assert_eq!(message.stop_reason, StopReason::Aborted);
        assert_eq!(message.error_message.as_deref(), Some("Request aborted"));
        assert_eq!(payload_calls.load(Ordering::Relaxed), 1);
        assert_eq!(fetch.calls.load(Ordering::Relaxed), 0);

        let mut missing_key = OpenAICompletionsOptions::default();
        missing_key.stream.request.signal = Some(signal);
        let message = run_message(
            &model("https://example.invalid/v1".to_owned()),
            &context(),
            missing_key,
        )
        .await;
        assert_eq!(
            message.error_message.as_deref(),
            Some("No API key for provider: openai")
        );
    }

    /// Pins pi OpenAI SDK `core/streaming.js:73-82` and
    /// `src/api/openai-completions.ts:640-646`: a mid-stream abort silently ends
    /// iteration, closes every open block, then emits `Request was aborted`.
    #[tokio::test]
    async fn midstream_abort_finishes_open_blocks_before_the_error_event() {
        let body = [
            json!({"choices":[{"delta":{"content":"answer"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"reasoning_content":"thought"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{
                "index":0,"id":"call-1","function":{"name":"lookup","arguments":"{}"}
            }]},"finish_reason":null}]}),
        ]
        .into_iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        .into_bytes();
        let signal = Arc::new(ManualAbort::default());
        let mut request_options = options();
        request_options.stream.request.signal = Some(signal.clone());
        request_options.stream.request.fetch = Some(Arc::new(PendingStaticFetch(body)));
        let mut events = stream(
            &model("https://example.invalid/v1".to_owned()),
            &context(),
            request_options,
        );
        let mut kinds = Vec::new();
        while let Some(event) = events.next().await {
            let kind = match event {
                AssistantMessageEvent::Start => "start",
                AssistantMessageEvent::TextStart { .. } => "text_start",
                AssistantMessageEvent::TextDelta { .. } => "text_delta",
                AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
                AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
                AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
                AssistantMessageEvent::ToolCallDelta { .. } => {
                    signal.abort();
                    "toolcall_delta"
                }
                AssistantMessageEvent::TextEnd { .. } => "text_end",
                AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
                AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
                AssistantMessageEvent::Error { reason, ref error } => {
                    assert_eq!(reason, ErrorStopReason::Aborted);
                    assert_eq!(error.stop_reason, StopReason::Aborted);
                    assert_eq!(error.error_message.as_deref(), Some("Request was aborted"));
                    "error"
                }
                AssistantMessageEvent::Done { .. } => "done",
            };
            kinds.push(kind);
        }
        assert_eq!(
            kinds,
            [
                "start",
                "text_start",
                "text_delta",
                "thinking_start",
                "thinking_delta",
                "toolcall_start",
                "toolcall_delta",
                "text_end",
                "thinking_end",
                "toolcall_end",
                "error",
            ]
        );
    }

    /// Ports retry.test.ts default no-retry behavior with real socket time.
    #[tokio::test]
    async fn provider_does_not_retry_by_default() {
        let no_retry_server = start_server(
            vec![
                Script::error(StatusCode::INTERNAL_SERVER_ERROR, "server error"),
                Script::sse(vec![stop_chunk("test-model")]),
            ],
            false,
        )
        .await;
        let no_retry_model = model(no_retry_server.base_url.clone());
        let mut no_retry_stream = stream(&no_retry_model, &context(), options());
        while no_retry_stream.next().await.is_some() {}
        let no_retry_message = no_retry_stream.result().await.unwrap();
        assert_eq!(no_retry_message.stop_reason, StopReason::Error);
        assert_eq!(no_retry_server.captured.lock().unwrap().len(), 1);
    }

    /// Ports retry.test.ts provider-owned initial request retries.
    #[tokio::test]
    async fn provider_retries_wrap_initial_call_only() {
        let server = start_server(
            vec![
                Script::error(StatusCode::TOO_MANY_REQUESTS, "rate limited"),
                Script::error(StatusCode::INTERNAL_SERVER_ERROR, "server error"),
                Script::sse(vec![stop_chunk("test-model")]),
            ],
            false,
        )
        .await;
        let mut request_options = options();
        request_options.stream.request.max_retries = Some(2.0);
        let mut event_stream = stream(&model(server.base_url.clone()), &context(), request_options);
        while event_stream.next().await.is_some() {}
        let message = event_stream.result().await.unwrap();
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(server.captured.lock().unwrap().len(), 3);
    }

    /// Pins pi openai-completions.ts:711-753 client-header behavior and
    /// samplingParams caller-wins ordering where the source has no focused test.
    #[tokio::test]
    async fn copilot_headers_and_sampling_overrides_match_pi_source() {
        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.provider = "github-copilot".into();
        let mut request_options = options();
        request_options
            .stream
            .sampling_params
            .get_or_insert_with(Map::new)
            .insert("model".to_owned(), Value::String("caller-model".to_owned()));
        request_options.stream.request.headers = Some(ProviderHeaders::from([(
            "Authorization".to_owned(),
            Some("Bearer upstream-token".to_owned()),
        )]));
        let (_, captured) = run_and_capture(test_model, context(), request_options, &server).await;
        assert_eq!(captured.body["model"], "caller-model");
        assert_eq!(
            captured.headers.get("x-initiator").map(String::as_str),
            Some("user")
        );
        assert_eq!(
            captured.headers.get("openai-intent").map(String::as_str),
            Some("conversation-edits")
        );
        assert_eq!(
            captured.headers.get("authorization").map(String::as_str),
            Some("Bearer upstream-token")
        );
        assert_eq!(
            captured.headers.get("user-agent").map(String::as_str),
            Some(get_pi_user_agent().as_str())
        );
    }

    /// Pins pi `src/api/openai-completions.ts:265-276`: the adapter echoes `model.api`
    /// and does not reject a model before issuing the Completions request.
    #[tokio::test]
    async fn model_api_is_echo_only() {
        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut test_model = model(server.base_url.clone());
        test_model.api = "custom-completions-alias".into();
        let message = run_message(&test_model, &context(), options()).await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.api.as_str(), "custom-completions-alias");
        assert_eq!(server.captured.lock().unwrap().len(), 1);
    }

    /// Pins pi `src/api/openai-completions.ts:1221-1226,1336-1348`: an assistant
    /// tool-call turn without text carries an explicit JSON `content: null`.
    #[test]
    fn assistant_tool_call_without_text_serializes_explicit_null_content() {
        let test_model = model("https://example.invalid/v1".to_owned());
        let mut assistant =
            AssistantMessage::pending("openai-completions", "openai", "test-model", 2);
        assistant.content = vec![AssistantContent::ToolCall(ToolCall::new(
            "call-1",
            "read",
            Map::new(),
        ))]
        .into();
        assistant.stop_reason = StopReason::ToolUse;
        let test_context = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(Box::new(assistant))],
            tools: None,
        };
        let messages = convert_messages(&test_model, &test_context).unwrap();
        assert!(messages[0].get("content").is_some());
        assert!(messages[0]["content"].is_null());
        assert!(messages[0]["tool_calls"].is_array());
    }

    /// Pins pi `src/api/openai-completions.ts:839-845`: qwen uses nullish
    /// fallback for an explicit-null map entry while zai uses defined semantics.
    #[test]
    fn qwen_explicit_null_effort_falls_back_to_the_level_name() {
        let mut test_model = model("https://example.invalid/v1".to_owned());
        test_model.reasoning = true;
        test_model.thinking_level_map = Some(ThinkingLevelMap {
            high: Some(None),
            ..ThinkingLevelMap::default()
        });
        let mut request_options = options();
        request_options.reasoning_effort = Some(ThinkingLevel::High);

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::Qwen),
            supports_reasoning_effort: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        assert_eq!(
            payload(&test_model, &context(), &request_options)["reasoning_effort"],
            "high"
        );

        test_model.compat = completions_compat(OpenAICompletionsCompat {
            thinking_format: Some(ThinkingFormat::Zai),
            supports_reasoning_effort: Some(true),
            ..OpenAICompletionsCompat::default()
        });
        assert!(
            payload(&test_model, &context(), &request_options)
                .get("reasoning_effort")
                .is_none()
        );
    }

    /// Pins pi `src/api/openai-completions.ts:711-753`: caller authorization
    /// replaces SDK bearer auth and reaches the server as exactly one header line.
    #[tokio::test]
    async fn custom_authorization_is_sent_exactly_once() {
        let server = start_server(vec![Script::sse(vec![stop_chunk("test-model")])], false).await;
        let mut request_options = options();
        request_options.stream.request.headers = Some(ProviderHeaders::from([(
            "authorization".to_owned(),
            Some("Bearer caller-token".to_owned()),
        )]));
        let (_, captured) = run_and_capture(
            model(server.base_url.clone()),
            context(),
            request_options,
            &server,
        )
        .await;
        let authorization = captured
            .header_lines
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .collect::<Vec<_>>();
        assert_eq!(authorization.len(), 1);
        assert_eq!(authorization[0].1, "Bearer caller-token");
    }

    /// Pins pi `src/api/openai-completions.ts:311-320`: `onResponse` observes
    /// the successful response's real status and headers before stream events.
    #[tokio::test]
    async fn on_response_receives_real_status_and_headers() {
        let mut script = Script::sse(vec![stop_chunk("test-model")]);
        script.status = StatusCode::CREATED;
        script
            .headers
            .insert("x-provider-response".to_owned(), "real".to_owned());
        let server = start_server(vec![script], false).await;
        let observed = Arc::new(Mutex::new(None::<ProviderResponse>));
        let callback = observed.clone();
        let mut request_options = options();
        request_options.stream.request.on_response = Some(Arc::new(move |response, _| {
            let callback = callback.clone();
            Box::pin(async move {
                *callback.lock().unwrap_or_else(PoisonError::into_inner) = Some(response);
                Ok(())
            })
        }));
        let message =
            run_message(&model(server.base_url.clone()), &context(), request_options).await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        let response = observed.lock().unwrap();
        let response = response.as_ref().unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(
            response.headers.get("x-provider-response"),
            Some(&"real".to_owned())
        );
    }

    /// Pins pi `src/api/openai-completions.ts:672-681` and
    /// `src/utils/error-body.ts:38-53,128-135`: parsed and raw failures retain
    /// exact provider body data, params, and the OpenRouter raw appendix.
    #[tokio::test]
    async fn provider_error_strings_preserve_json_and_non_json_bodies() {
        let json_body = r#"{"error":{"message":"bad request","type":"invalid_request_error","param":"tools[0]","code":"bad","metadata":{"raw":{"upstream":"detail"}}}}"#;
        let json_server = start_server(
            vec![Script::raw_error(StatusCode::BAD_REQUEST, json_body)],
            false,
        )
        .await;
        let json_error =
            run_message(&model(json_server.base_url.clone()), &context(), options()).await;
        assert_eq!(
            json_error.error_message.as_deref(),
            Some(concat!(
                r#"400: {"message":"bad request","type":"invalid_request_error","param":"tools[0]","code":"bad","metadata":{"raw":{"upstream":"detail"}}}"#,
                "\n[object Object]"
            ))
        );

        let text_server = start_server(
            vec![Script::raw_error(
                StatusCode::BAD_GATEWAY,
                "upstream exploded",
            )],
            false,
        )
        .await;
        let text_error =
            run_message(&model(text_server.base_url.clone()), &context(), options()).await;
        assert_eq!(
            text_error.error_message.as_deref(),
            Some("502 upstream exploded")
        );
    }

    /// Pins pi `src/utils/provider-retry.ts:22-67,105-123`: response retry
    /// headers control classification, delay, and the maximum-delay fail-fast.
    #[tokio::test]
    async fn response_headers_drive_retry_delay_classification_and_cap() {
        let mut seconds = Script::error(StatusCode::TOO_MANY_REQUESTS, "retry after seconds");
        seconds
            .headers
            .insert("retry-after".to_owned(), "61".to_owned());
        let seconds_server = start_server(vec![seconds], false).await;
        let mut request_options = options();
        request_options.stream.request.max_retries = Some(1.0);
        request_options.stream.request.max_retry_delay_ms = Some(60_000.0);
        assert_eq!(
            run_message(
                &model(seconds_server.base_url.clone()),
                &context(),
                request_options,
            )
            .await
            .error_message
            .as_deref(),
            Some("Server requested 61s retry delay (max: 60s). 429 retry after seconds")
        );
        assert_eq!(seconds_server.captured.lock().unwrap().len(), 1);

        let mut delayed = Script::error(StatusCode::TOO_MANY_REQUESTS, "rate limited");
        delayed
            .headers
            .insert("retry-after-ms".to_owned(), "40".to_owned());
        let delayed_server = start_server(
            vec![delayed, Script::sse(vec![stop_chunk("test-model")])],
            false,
        )
        .await;
        let mut request_options = options();
        request_options.stream.request.max_retries = Some(1.0);
        let started = std::time::Instant::now();
        let message = run_message(
            &model(delayed_server.base_url.clone()),
            &context(),
            request_options,
        )
        .await;
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert!(started.elapsed() >= std::time::Duration::from_millis(30));

        let mut forced = Script::error(StatusCode::BAD_REQUEST, "retry forced");
        forced
            .headers
            .insert("x-should-retry".to_owned(), "true".to_owned());
        let forced_server = start_server(
            vec![forced, Script::sse(vec![stop_chunk("test-model")])],
            false,
        )
        .await;
        let mut request_options = options();
        request_options.stream.request.max_retries = Some(1.0);
        assert_eq!(
            run_message(
                &model(forced_server.base_url.clone()),
                &context(),
                request_options,
            )
            .await
            .stop_reason,
            StopReason::Stop
        );

        let mut denied = Script::error(StatusCode::INTERNAL_SERVER_ERROR, "retry denied");
        denied
            .headers
            .insert("x-should-retry".to_owned(), "false".to_owned());
        let denied_server = start_server(
            vec![denied, Script::sse(vec![stop_chunk("test-model")])],
            false,
        )
        .await;
        let mut request_options = options();
        request_options.stream.request.max_retries = Some(1.0);
        assert_eq!(
            run_message(
                &model(denied_server.base_url.clone()),
                &context(),
                request_options,
            )
            .await
            .stop_reason,
            StopReason::Error
        );
        assert_eq!(denied_server.captured.lock().unwrap().len(), 1);

        let mut capped = Script::error(StatusCode::TOO_MANY_REQUESTS, "rate limited");
        capped
            .headers
            .insert("retry-after-ms".to_owned(), "61000".to_owned());
        let capped_server = start_server(vec![capped], false).await;
        let mut request_options = options();
        request_options.stream.request.max_retries = Some(1.0);
        request_options.stream.request.max_retry_delay_ms = Some(60_000.0);
        let message = run_message(
            &model(capped_server.base_url.clone()),
            &context(),
            request_options,
        )
        .await;
        assert_eq!(
            message.error_message.as_deref(),
            Some("Server requested 61s retry delay (max: 60s). 429 rate limited")
        );
    }

    /// Pins pi `src/api/openai-completions.ts:311-319,505-550`: the SDK-backed
    /// SSE path preserves a UTF-8 scalar split across network chunks.
    #[tokio::test]
    async fn split_multibyte_sse_delta_is_byte_correct() {
        let event = json!({
            "id":"chatcmpl-utf8","model":"test-model",
            "choices":[{"index":0,"delta":{"content":"café"},"finish_reason":"stop"}]
        });
        let wire = format!("data: {event}\n\ndata: [DONE]\n\n").into_bytes();
        let split = wire
            .windows(2)
            .position(|window| window == "é".as_bytes())
            .unwrap()
            + 1;
        let server = start_server(
            vec![Script::raw_sse(vec![
                wire[..split].to_vec(),
                wire[split..].to_vec(),
            ])],
            false,
        )
        .await;
        let message = run_message(&model(server.base_url.clone()), &context(), options()).await;
        let Some(AssistantContent::Text(text)) = message.content.first() else {
            panic!("text response");
        };
        assert_eq!(text.text, "café");
    }

    /// Pins pi `src/api/openai-completions.ts:311-319,505-550`: the OpenAI SDK
    /// concatenates multiple `data:` fields in one SSE event with a newline.
    #[tokio::test]
    async fn multiline_data_sse_event_is_concatenated_before_json_decode() {
        let wire = concat!(
            "data: {\"id\":\"chatcmpl-multiline\",\"model\":\"test-model\",\n",
            "data: \"choices\":[{\"index\":0,\"delta\":{\"content\":\"joined\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let server =
            start_server(vec![Script::raw_sse(vec![wire.as_bytes().to_vec()])], false).await;
        let message = run_message(&model(server.base_url.clone()), &context(), options()).await;
        let Some(AssistantContent::Text(text)) = message.content.first() else {
            panic!("text response");
        };
        assert_eq!(text.text, "joined");
    }

    /// Pins pi `src/api/openai-completions.ts:265-272,664-708`: neither public
    /// stream entry point synchronously throws when no async runtime is active.
    #[test]
    fn stream_entry_points_without_tokio_runtime_return_terminal_errors() {
        let test_model = model("https://example.invalid/v1".to_owned());
        let streams = [
            stream(&test_model, &context(), options()),
            stream_simple(&test_model, &context(), SimpleStreamOptions::default()),
        ];
        for mut events in streams {
            let event = futures::executor::block_on(events.next()).unwrap();
            assert!(matches!(event, AssistantMessageEvent::Error { .. }));
            let message = futures::executor::block_on(events.result()).unwrap();
            assert_eq!(message.stop_reason, StopReason::Error);
            assert_eq!(
                message.error_message.as_deref(),
                Some("Tokio runtime is not available")
            );
        }
    }
}
