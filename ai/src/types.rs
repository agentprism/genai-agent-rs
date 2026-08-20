//! Provider-neutral data contracts mirrored from pi `src/types.ts`.

use futures::future::BoxFuture;
use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

macro_rules! open_string_newtype {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

open_string_newtype!(Api);
open_string_newtype!(ProviderId);
open_string_newtype!(ImagesApi);
open_string_newtype!(ImagesProviderId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolChoice {
    Auto,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

fn deserialize_present_json<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

fn serialize_text_signature_version<S>(version: &u8, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if *version != 1 {
        return Err(S::Error::custom("text signature version must be 1"));
    }
    serializer.serialize_u8(*version)
}

fn deserialize_text_signature_version<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u8::deserialize(deserializer)?;
    if version != 1 {
        return Err(D::Error::custom("text signature version must be 1"));
    }
    Ok(version)
}

/// Missing keys inherit provider defaults; a present `null` marks a level unsupported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingLevelMap {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub off: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub minimal: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub low: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub medium: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub high: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub xhigh: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub max: Option<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingVariable {
    #[serde(rename = "thinking.enabled")]
    Enabled,
    #[serde(rename = "thinking.effort")]
    Effort,
    #[serde(rename = "thinking.budget")]
    Budget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatTemplateKwargValue {
    String(String),
    Number(Number),
    Boolean(bool),
    Null,
    Variable {
        #[serde(rename = "$var")]
        variable: ThinkingVariable,
        #[serde(
            rename = "omitWhenOff",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        omit_when_off: Option<bool>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingTokenBudgetField {
    ThinkingTokenBudget,
    ThinkingBudget,
    ThinkingBudgetTokens,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}

pub type ProviderEnv = BTreeMap<String, String>;
pub type ProviderHeaders = BTreeMap<String, Option<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionAffinityFormat {
    Openai,
    OpenaiNosession,
    Openrouter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
}

pub trait AbortSignal: Send + Sync {
    fn is_aborted(&self) -> bool;
    fn cancelled(&self) -> BoxFuture<'_, ()>;
}

pub trait TelemetryContext: Send + Sync {}

pub type ProviderBodyStream =
    Pin<Box<dyn futures::Stream<Item = Result<Vec<u8>, String>> + Send + 'static>>;

#[derive(Clone)]
pub struct ProviderHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub signal: Option<Arc<dyn AbortSignal>>,
}

impl fmt::Debug for ProviderHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderHttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .field("signal", &self.signal.is_some())
            .finish()
    }
}

pub struct ProviderHttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<ProviderBodyStream>,
}

pub trait FetchFunction: Send + Sync {
    fn fetch(
        &self,
        request: ProviderHttpRequest,
    ) -> BoxFuture<'_, Result<ProviderHttpResponse, String>>;
}

pub type OnPayload<TModel = Model> =
    Arc<dyn for<'a> Fn(Value, &'a TModel) -> BoxFuture<'a, Option<Value>> + Send + Sync + 'static>;
pub type OnResponse<TModel = Model> =
    Arc<dyn for<'a> Fn(ProviderResponse, &'a TModel) -> BoxFuture<'a, ()> + Send + Sync + 'static>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", bound(serialize = "", deserialize = ""))]
pub struct ProviderRequestOptions<TModel = Model> {
    #[serde(skip)]
    pub signal: Option<Arc<dyn AbortSignal>>,
    #[serde(skip)]
    pub telemetry_context: Option<Arc<dyn TelemetryContext>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip)]
    pub fetch: Option<Arc<dyn FetchFunction>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
    #[serde(skip)]
    pub on_payload: Option<OnPayload<TModel>>,
    #[serde(skip)]
    pub on_response: Option<OnResponse<TModel>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
}

impl<TModel> Default for ProviderRequestOptions<TModel> {
    fn default() -> Self {
        Self {
            signal: None,
            telemetry_context: None,
            api_key: None,
            fetch: None,
            env: None,
            on_payload: None,
            on_response: None,
            headers: None,
            timeout_ms: None,
            max_retries: None,
            max_retry_delay_ms: None,
        }
    }
}

impl<TModel> fmt::Debug for ProviderRequestOptions<TModel> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRequestOptions")
            .field("signal", &self.signal.is_some())
            .field("telemetry_context", &self.telemetry_context.is_some())
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("fetch", &self.fetch.is_some())
            .field("env", &self.env)
            .field("on_payload", &self.on_payload.is_some())
            .field("on_response", &self.on_response.is_some())
            .field("headers", &self.headers)
            .field("timeout_ms", &self.timeout_ms)
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .finish()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamOptions {
    #[serde(flatten)]
    pub request: ProviderRequestOptions<Model>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderStreamOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredFetchOptions {
    #[serde(flatten)]
    pub request: ProviderRequestOptions<Model>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u64>,
}

pub type DeferredCancelOptions = ProviderRequestOptions<Model>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeferredWindow {
    #[serde(rename = "15m")]
    Minutes15,
    #[serde(rename = "1h")]
    Hour1,
    #[serde(rename = "24h")]
    Hours24,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeferredRequest {
    Enabled(bool),
    Window {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<DeferredWindow>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleStreamOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgets>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSignatureV1 {
    #[serde(
        serialize_with = "serialize_text_signature_version",
        deserialize_with = "deserialize_text_signature_version"
    )]
    pub v: u8,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<TextSignaturePhase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSignaturePhase {
    Commentary,
    FinalAnswer,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextContentType {
    #[default]
    #[serde(rename = "text")]
    Text,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    #[serde(rename = "type")]
    pub kind: TextContentType,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

impl TextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: TextContentType::Text,
            text: text.into(),
            text_signature: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingContentType {
    #[default]
    #[serde(rename = "thinking")]
    Thinking,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    #[serde(rename = "type")]
    pub kind: ThinkingContentType,
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

impl ThinkingContent {
    pub fn new(thinking: impl Into<String>) -> Self {
        Self {
            kind: ThinkingContentType::Thinking,
            thinking: thinking.into(),
            thinking_signature: None,
            redacted: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageContentType {
    #[default]
    #[serde(rename = "image")]
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    #[serde(rename = "type")]
    pub kind: ImageContentType,
    pub data: String,
    pub mime_type: String,
}

impl ImageContent {
    pub fn new(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            kind: ImageContentType::Image,
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCallType {
    #[default]
    #[serde(rename = "toolCall")]
    ToolCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    #[serde(rename = "type")]
    pub kind: ToolCallType,
    pub id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Map<String, Value>,
    ) -> Self {
        Self {
            kind: ToolCallType::ToolCall,
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signature: None,
            namespace: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContentBlock {
    Text(TextContent),
    Image(ImageContent),
}

pub type ToolResultContent = UserContentBlock;
pub type ImagesInputContent = UserContentBlock;
pub type ImagesOutputContent = UserContentBlock;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<UserContentBlock>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserRole {
    #[default]
    #[serde(rename = "user")]
    User,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub role: UserRole,
    pub content: UserContent,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantRole {
    #[default]
    #[serde(rename = "assistant")]
    Assistant,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolResultRole {
    #[default]
    #[serde(rename = "toolResult")]
    ToolResult,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    #[default]
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
    #[serde(rename = "deferred")]
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuccessfulStopReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "deferred")]
    Deferred,
}

impl From<SuccessfulStopReason> for StopReason {
    fn from(reason: SuccessfulStopReason) -> Self {
        match reason {
            SuccessfulStopReason::Stop => Self::Stop,
            SuccessfulStopReason::Length => Self::Length,
            SuccessfulStopReason::ToolUse => Self::ToolUse,
            SuccessfulStopReason::Deferred => Self::Deferred,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorStopReason {
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
}

impl From<ErrorStopReason> for StopReason {
    fn from(reason: ErrorStopReason) -> Self {
        match reason {
            ErrorStopReason::Error => Self::Error,
            ErrorStopReason::Aborted => Self::Aborted,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DiagnosticCode {
    String(String),
    Number(Number),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticErrorInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<DiagnosticCode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessageDiagnostic {
    #[serde(rename = "type")]
    pub kind: String,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<DiagnosticErrorInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_json"
    )]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub role: AssistantRole,
    pub content: Vec<AssistantContent>,
    pub api: Api,
    pub provider: ProviderId,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<AssistantMessageDiagnostic>>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    pub timestamp: i64,
}

impl AssistantMessage {
    pub fn pending(
        api: impl Into<Api>,
        provider: impl Into<ProviderId>,
        model: impl Into<String>,
        timestamp: i64,
    ) -> Self {
        Self {
            role: AssistantRole::Assistant,
            content: Vec::new(),
            api: api.into(),
            provider: provider.into(),
            model: model.into(),
            response_model: None,
            response_id: None,
            reasoning_details: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub role: ToolResultRole,
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ToolResultContent>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_json"
    )]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    pub is_error: bool,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    User(Box<UserMessage>),
    Assistant(Box<AssistantMessage>),
    ToolResult(Box<ToolResultMessage>),
}

pub type JsonValue = Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrammarFormat {
    OpenaiLark,
    OpenaiRegex,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrammarVariants {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_lark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_regex: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StrictPreference {
    Prefer,
    Require,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSamplingConfig {
    JsonSchema { strict: StrictPreference },
    Grammar { variants: GrammarVariants },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolConstrainedSampling {
    Disabled,
    Config(ConstrainedSamplingConfig),
}

impl Serialize for ToolConstrainedSampling {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Disabled => false.serialize(serializer),
            Self::Config(config) => config.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ToolConstrainedSampling {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if value == Value::Bool(false) {
            return Ok(Self::Disabled);
        }
        if value == Value::Bool(true) {
            return Err(D::Error::custom(
                "constrainedSampling accepts false, not true",
            ));
        }
        serde_json::from_value(value)
            .map(Self::Config)
            .map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<ToolConstrainedSampling>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImagesContext {
    pub input: Vec<ImagesInputContent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImagesStopReason {
    Stop,
    Error,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: ImagesApi,
    pub provider: ImagesProviderId,
    pub model: String,
    pub output: Vec<ImagesOutputContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: ImagesStopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelInput {
    Text,
    Image,
}

pub type ImagesOutput = ModelInput;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    pub input_tokens_above: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicAllowedFallbackModel {
    pub provider: ProviderId,
    pub model: String,
    pub cost: ModelCost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThinkingFormat {
    #[serde(rename = "openai")]
    Openai,
    #[serde(rename = "openrouter")]
    Openrouter,
    #[serde(rename = "deepseek")]
    Deepseek,
    #[serde(rename = "together")]
    Together,
    #[serde(rename = "baseten")]
    Baseten,
    #[serde(rename = "zai")]
    Zai,
    #[serde(rename = "qwen")]
    Qwen,
    #[serde(rename = "chat-template")]
    ChatTemplate,
    #[serde(rename = "qwen-chat-template")]
    QwenChatTemplate,
    #[serde(rename = "string-thinking")]
    StringThinking,
    #[serde(rename = "ant-ling")]
    AntLing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaxTokensField {
    #[serde(rename = "max_completion_tokens")]
    MaxCompletionTokens,
    #[serde(rename = "max_tokens")]
    MaxTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheControlFormat {
    #[serde(rename = "anthropic")]
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferredToolsMode {
    #[serde(rename = "kimi")]
    Kimi,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NumberOrString {
    Number(f64),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoutingSort {
    Name(String),
    Options(RoutingSortOptions),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutingSortOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_option"
    )]
    pub partition: Option<Option<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterMaxPrice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<NumberOrString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<NumberOrString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<NumberOrString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<NumberOrString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<NumberOrString>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PercentileThresholds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p75: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoutingThreshold {
    Number(f64),
    Percentiles(PercentileThresholds),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenRouterRouting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<DataCollection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforce_distillable_text: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantizations: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<RoutingSort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_price: Option<OpenRouterMaxPrice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_min_throughput: Option<RoutingThreshold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_max_latency: Option<RoutingThreshold>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataCollection {
    Deny,
    Allow,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VercelGatewayRouting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICompletionsCompat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_finish_reason: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_tool_result_name: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_assistant_after_tool_result: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_thinking_as_text: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<ThinkingFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<BTreeMap<String, ChatTemplateKwargValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_args: Option<BTreeMap<String, ChatTemplateKwargValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_router_routing: Option<OpenRouterRouting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vercel_gateway_routing: Option<VercelGatewayRouting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zai_tool_stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_token_budget_field: Option<ThinkingTokenBudgetField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking_token_budget: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "supportsOpenAIGrammarTools")]
    pub supports_open_ai_grammar_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control_format: Option<CacheControlFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_session_affinity_headers: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_tools_mode: Option<DeferredToolsMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_affinity_format: Option<SessionAffinityFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAIResponsesCompat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_affinity_format: Option<SessionAffinityFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "supportsOpenAIGrammarTools")]
    pub supports_open_ai_grammar_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_additional_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tool_search: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_explicit_prompt_cache_mode: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicMessagesCompat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_eager_tool_input_streaming: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_long_cache_retention: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_session_affinity_headers: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_cache_control_on_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_empty_signature: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_strict_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_fallback_models: Option<Vec<AnthropicAllowedFallbackModel>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tool_references: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockCompat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ModelCompat {
    OpenAICompletions(Box<OpenAICompletionsCompat>),
    OpenAIResponses(OpenAIResponsesCompat),
    AnthropicMessages(AnthropicMessagesCompat),
    Bedrock(BedrockCompat),
}

impl ModelCompat {
    fn matches_api(&self, api: &str) -> bool {
        match self {
            Self::OpenAICompletions(_) => api == "openai-completions",
            Self::OpenAIResponses(_) => matches!(
                api,
                "openai-responses" | "azure-openai-responses" | "openai-codex-responses"
            ),
            Self::AnthropicMessages(_) => api == "anthropic-messages",
            Self::Bedrock(_) => api == "bedrock-converse-stream",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    pub base_url: String,
    pub reasoning: bool,
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<ModelInput>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    pub sampling_params: Option<Map<String, Value>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<ModelCompat>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelWireRef<'a> {
    id: &'a str,
    name: &'a str,
    api: &'a Api,
    provider: &'a ProviderId,
    base_url: &'a str,
    reasoning: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level_map: &'a Option<ThinkingLevelMap>,
    input: &'a [ModelInput],
    cost: &'a ModelCost,
    context_window: u64,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    sampling_params: &'a Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: &'a Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compat: &'a Option<ModelCompat>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelWireOwned {
    id: String,
    name: String,
    api: Api,
    provider: ProviderId,
    base_url: String,
    reasoning: bool,
    #[serde(default)]
    thinking_level_map: Option<ThinkingLevelMap>,
    input: Vec<ModelInput>,
    cost: ModelCost,
    context_window: u64,
    max_tokens: u64,
    #[serde(default)]
    sampling_params: Option<Map<String, Value>>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    compat: Option<Value>,
}

impl Serialize for Model {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self
            .compat
            .as_ref()
            .is_some_and(|compat| !compat.matches_api(self.api.as_str()))
        {
            return Err(S::Error::custom(format!(
                "compat variant does not match model api {}",
                self.api
            )));
        }
        ModelWireRef {
            id: &self.id,
            name: &self.name,
            api: &self.api,
            provider: &self.provider,
            base_url: &self.base_url,
            reasoning: self.reasoning,
            thinking_level_map: &self.thinking_level_map,
            input: &self.input,
            cost: &self.cost,
            context_window: self.context_window,
            max_tokens: self.max_tokens,
            sampling_params: &self.sampling_params,
            headers: &self.headers,
            compat: &self.compat,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Model {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelWireOwned::deserialize(deserializer)?;
        let compat = match (wire.api.as_str(), wire.compat) {
            (_, None) => None,
            ("openai-completions", Some(value)) => Some(ModelCompat::OpenAICompletions(Box::new(
                serde_json::from_value(value).map_err(D::Error::custom)?,
            ))),
            (
                "openai-responses" | "azure-openai-responses" | "openai-codex-responses",
                Some(value),
            ) => Some(ModelCompat::OpenAIResponses(
                serde_json::from_value(value).map_err(D::Error::custom)?,
            )),
            ("anthropic-messages", Some(value)) => Some(ModelCompat::AnthropicMessages(
                serde_json::from_value(value).map_err(D::Error::custom)?,
            )),
            ("bedrock-converse-stream", Some(value)) => Some(ModelCompat::Bedrock(
                serde_json::from_value(value).map_err(D::Error::custom)?,
            )),
            (api, Some(_)) => {
                return Err(D::Error::custom(format!(
                    "model api {api} has no compatible compat family"
                )));
            }
        };
        Ok(Self {
            id: wire.id,
            name: wire.name,
            api: wire.api,
            provider: wire.provider,
            base_url: wire.base_url,
            reasoning: wire.reasoning,
            thinking_level_map: wire.thinking_level_map,
            input: wire.input,
            cost: wire.cost,
            context_window: wire.context_window,
            max_tokens: wire.max_tokens,
            sampling_params: wire.sampling_params,
            headers: wire.headers,
            compat,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesModel {
    pub id: String,
    pub name: String,
    pub api: ImagesApi,
    pub provider: ImagesProviderId,
    pub base_url: String,
    pub input: Vec<ModelInput>,
    pub output: Vec<ImagesOutput>,
    pub cost: ModelCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesOptions {
    #[serde(flatten)]
    pub request: ProviderRequestOptions<ImagesModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderImagesOptions {
    #[serde(flatten)]
    pub images: ImagesOptions,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub type StreamFunction = Arc<
    dyn Fn(
            &Model,
            &Context,
            Option<StreamOptions>,
        ) -> crate::event_stream::AssistantMessageEventStream
        + Send
        + Sync
        + 'static,
>;

pub type ImagesError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type ImagesFunction = Arc<
    dyn for<'a> Fn(
            &'a ImagesModel,
            &'a ImagesContext,
            Option<ImagesOptions>,
        ) -> BoxFuture<'a, Result<AssistantImages, ImagesError>>
        + Send
        + Sync
        + 'static,
>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_round_trip<T>(value: &T, expected: Value)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug,
    {
        assert_eq!(serde_json::to_value(value).unwrap(), expected);
        let decoded = serde_json::from_value::<T>(expected).unwrap();
        assert_eq!(&decoded, value);
    }

    fn full_usage() -> Usage {
        Usage {
            input: 11,
            output: 7,
            cache_read: 3,
            cache_write: 5,
            cache_write_1h: Some(0),
            reasoning: Some(0),
            total_tokens: 18,
            cost: UsageCost {
                input: 0.1,
                output: 0.2,
                cache_read: 0.03,
                cache_write: 0.05,
                total: 0.38,
            },
        }
    }

    /// Pins pi `types.ts:344-380` content discriminants, camelCase fidelity fields, and omission.
    #[test]
    fn content_blocks_round_trip_exact_wire_json() {
        let signature = TextSignatureV1 {
            v: 1,
            id: "item-1".into(),
            phase: Some(TextSignaturePhase::FinalAnswer),
        };
        assert_round_trip(
            &signature,
            json!({"v":1,"id":"item-1","phase":"final_answer"}),
        );
        assert!(serde_json::from_value::<TextSignatureV1>(json!({"v":2,"id":"item-1"})).is_err());
        assert!(
            serde_json::to_value(TextSignatureV1 {
                v: 2,
                id: "item-1".into(),
                phase: None,
            })
            .is_err()
        );

        let text = TextContent {
            kind: TextContentType::Text,
            text: "answer".into(),
            text_signature: Some("txt-sig".into()),
        };
        assert_round_trip(
            &text,
            json!({"type":"text","text":"answer","textSignature":"txt-sig"}),
        );

        let thinking = ThinkingContent {
            kind: ThinkingContentType::Thinking,
            thinking: "hidden".into(),
            thinking_signature: Some("opaque".into()),
            redacted: Some(false),
        };
        assert_round_trip(
            &thinking,
            json!({"type":"thinking","thinking":"hidden","thinkingSignature":"opaque","redacted":false}),
        );

        let image = ImageContent::new("aGVsbG8=", "image/png");
        assert_round_trip(
            &image,
            json!({"type":"image","data":"aGVsbG8=","mimeType":"image/png"}),
        );

        let mut arguments = Map::new();
        arguments.insert("city".into(), json!("Paris"));
        let tool_call = ToolCall {
            thought_signature: Some("thought-sig".into()),
            namespace: Some("weather".into()),
            ..ToolCall::new("call-1", "forecast", arguments)
        };
        assert_round_trip(
            &tool_call,
            json!({"type":"toolCall","id":"call-1","name":"forecast","arguments":{"city":"Paris"},"thoughtSignature":"thought-sig","namespace":"weather"}),
        );

        assert_eq!(
            serde_json::to_value(TextContent::new("")).unwrap(),
            json!({"type":"text","text":""})
        );
        assert_eq!(
            serde_json::to_value(ThinkingContent::new("")).unwrap(),
            json!({"type":"thinking","thinking":""})
        );
    }

    /// Pins pi `types.ts:382-403`: optional counters preserve explicit zero and omit only absence.
    #[test]
    fn usage_round_trips_presence_bearing_counters() {
        assert_round_trip(
            &full_usage(),
            json!({
                "input":11,"output":7,"cacheRead":3,"cacheWrite":5,
                "cacheWrite1h":0,"reasoning":0,"totalTokens":18,
                "cost":{"input":0.1,"output":0.2,"cacheRead":0.03,"cacheWrite":0.05,"total":0.38}
            }),
        );
        assert_eq!(
            serde_json::to_value(Usage::default()).unwrap(),
            json!({
                "input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
                "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}
            })
        );
    }

    /// Pins pi `types.ts:421-425` user string and multimodal content union shapes.
    #[test]
    fn user_message_variants_round_trip_exact_wire_json() {
        let string_message = Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("hello".into()),
            timestamp: 10,
        }));
        assert_round_trip(
            &string_message,
            json!({"role":"user","content":"hello","timestamp":10}),
        );

        let blocks_message = Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Blocks(vec![
                UserContentBlock::Text(TextContent::new("look")),
                UserContentBlock::Image(ImageContent::new("AA==", "image/jpeg")),
            ]),
            timestamp: 11,
        }));
        assert_round_trip(
            &blocks_message,
            json!({"role":"user","content":[{"type":"text","text":"look"},{"type":"image","data":"AA==","mimeType":"image/jpeg"}],"timestamp":11}),
        );
    }

    /// Pins pi `types.ts:427-448` including every assistant fidelity and terminal field.
    #[test]
    fn assistant_message_round_trips_all_fidelity_fields() {
        let diagnostic = AssistantMessageDiagnostic {
            kind: "retry".into(),
            timestamp: 12,
            error: Some(DiagnosticErrorInfo {
                name: Some("ProviderError".into()),
                message: "retrying".into(),
                stack: Some("redacted stack".into()),
                code: Some(DiagnosticCode::Number(Number::from(429))),
            }),
            details: Some(Map::from_iter([("attempt".into(), json!(2))])),
        };
        let message = Message::Assistant(Box::new(AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![
                AssistantContent::Text(TextContent {
                    text_signature: Some("text-sig".into()),
                    ..TextContent::new("ok")
                }),
                AssistantContent::Thinking(ThinkingContent {
                    thinking_signature: Some("thinking-sig".into()),
                    redacted: Some(true),
                    ..ThinkingContent::new("ciphertext")
                }),
                AssistantContent::ToolCall(ToolCall {
                    thought_signature: Some("thought-sig".into()),
                    ..ToolCall::new("c", "t", Map::new())
                }),
            ],
            api: Api::from("openai-responses"),
            provider: ProviderId::from("custom-provider"),
            model: "requested".into(),
            response_model: Some("actual".into()),
            response_id: Some("resp-1".into()),
            reasoning_details: Some(vec![json!({"opaque":true}), Value::Null]),
            diagnostics: Some(vec![diagnostic]),
            usage: full_usage(),
            stop_reason: StopReason::Deferred,
            deferred: Some(DeferredHandle {
                provider: "custom-provider".into(),
                model_id: "requested".into(),
                api: "openai-responses".into(),
                id: "deferred-1".into(),
                expires_at: Some(99),
                poll_after_ms: Some(0),
                data: Some(json!({"row":1})),
            }),
            error_message: Some("detail".into()),
            raw_stop_reason: Some("provider_reason".into()),
            end_turn: Some(false),
            timestamp: 13,
        }));
        assert_round_trip(
            &message,
            json!({
                "role":"assistant",
                "content":[
                    {"type":"text","text":"ok","textSignature":"text-sig"},
                    {"type":"thinking","thinking":"ciphertext","thinkingSignature":"thinking-sig","redacted":true},
                    {"type":"toolCall","id":"c","name":"t","arguments":{},"thoughtSignature":"thought-sig"}
                ],
                "api":"openai-responses","provider":"custom-provider","model":"requested",
                "responseModel":"actual","responseId":"resp-1",
                "reasoningDetails":[{"opaque":true},null],
                "diagnostics":[{
                    "type":"retry","timestamp":12,
                    "error":{"name":"ProviderError","message":"retrying","stack":"redacted stack","code":429},
                    "details":{"attempt":2}
                }],
                "usage":{
                    "input":11,"output":7,"cacheRead":3,"cacheWrite":5,"cacheWrite1h":0,
                    "reasoning":0,"totalTokens":18,
                    "cost":{"input":0.1,"output":0.2,"cacheRead":0.03,"cacheWrite":0.05,"total":0.38}
                },
                "stopReason":"deferred",
                "deferred":{
                    "provider":"custom-provider","modelId":"requested","api":"openai-responses",
                    "id":"deferred-1","expiresAt":99,"pollAfterMs":0,"data":{"row":1}
                },
                "errorMessage":"detail","rawStopReason":"provider_reason","endTurn":false,"timestamp":13
            }),
        );

        let minimal = AssistantMessage::pending("custom-api", "custom-provider", "model", 0);
        assert_eq!(
            serde_json::to_value(minimal).unwrap(),
            json!({
                "role":"assistant","content":[],"api":"custom-api","provider":"custom-provider",
                "model":"model","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,
                "totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},
                "stopReason":"pending","timestamp":0
            })
        );
    }

    /// Pins pi `types.ts:450-466` tool-result text/image, details, usage, load-point, and error fields.
    #[test]
    fn tool_result_message_round_trips_exact_wire_json() {
        let message = Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-1".into(),
            tool_name: "vision".into(),
            content: vec![
                ToolResultContent::Text(TextContent::new("done")),
                ToolResultContent::Image(ImageContent::new("AA==", "image/png")),
            ],
            details: Some(json!({"exitCode":0})),
            usage: Some(full_usage()),
            added_tool_names: Some(vec!["later_tool".into()]),
            is_error: false,
            timestamp: 14,
        }));
        assert_round_trip(
            &message,
            json!({
                "role":"toolResult","toolCallId":"call-1","toolName":"vision",
                "content":[
                    {"type":"text","text":"done"},
                    {"type":"image","data":"AA==","mimeType":"image/png"}
                ],
                "details":{"exitCode":0},
                "usage":{
                    "input":11,"output":7,"cacheRead":3,"cacheWrite":5,"cacheWrite1h":0,
                    "reasoning":0,"totalTokens":18,
                    "cost":{"input":0.1,"output":0.2,"cacheRead":0.03,"cacheWrite":0.05,"total":0.38}
                },
                "addedToolNames":["later_tool"],"isError":false,"timestamp":14
            }),
        );
    }

    /// Pins pi `types.ts:436` and `utils/diagnostics.ts:1-13` diagnostic omission and code unions.
    #[test]
    fn diagnostics_round_trip_string_and_numeric_codes() {
        let numeric = AssistantMessageDiagnostic {
            kind: "provider".into(),
            timestamp: 1,
            error: Some(DiagnosticErrorInfo {
                name: None,
                message: "limited".into(),
                stack: None,
                code: Some(DiagnosticCode::Number(Number::from(429))),
            }),
            details: None,
        };
        assert_round_trip(
            &numeric,
            json!({"type":"provider","timestamp":1,"error":{"message":"limited","code":429}}),
        );
        let string = AssistantMessageDiagnostic {
            kind: "runtime".into(),
            timestamp: 2,
            error: Some(DiagnosticErrorInfo {
                name: Some("Error".into()),
                message: "failed".into(),
                stack: Some("stack".into()),
                code: Some(DiagnosticCode::String("E_FAIL".into())),
            }),
            details: Some(Map::new()),
        };
        assert_round_trip(
            &string,
            json!({"type":"runtime","timestamp":2,"error":{"name":"Error","message":"failed","stack":"stack","code":"E_FAIL"},"details":{}}),
        );
    }

    /// Pins pi `types.ts:83-85,112-114`: missing/null thinking values and null header sentinels differ.
    #[test]
    fn presence_bearing_maps_preserve_null_and_zero() {
        let levels: ThinkingLevelMap =
            serde_json::from_value(json!({"off":null,"low":"low"})).unwrap();
        assert_eq!(levels.off, Some(None));
        assert_eq!(levels.minimal, None);
        assert_eq!(
            serde_json::to_value(levels).unwrap(),
            json!({"off":null,"low":"low"})
        );

        let options: ProviderRequestOptions<Model> = ProviderRequestOptions {
            headers: Some(BTreeMap::from([
                ("x-default".into(), None),
                ("x-empty".into(), Some(String::new())),
            ])),
            max_retry_delay_ms: Some(0),
            ..ProviderRequestOptions::default()
        };
        assert_eq!(
            serde_json::to_value(options).unwrap(),
            json!({"headers":{"x-default":null,"x-empty":""},"maxRetryDelayMs":0})
        );

        let handle: DeferredHandle = serde_json::from_value(json!({
            "provider":"p","modelId":"m","api":"a","id":"d","data":null
        }))
        .unwrap();
        assert_eq!(handle.data, Some(Value::Null));
        assert_eq!(
            serde_json::to_value(handle).unwrap(),
            json!({"provider":"p","modelId":"m","api":"a","id":"d","data":null})
        );

        let tool_result: ToolResultMessage = serde_json::from_value(json!({
            "role":"toolResult","toolCallId":"c","toolName":"t","content":[],
            "details":null,"isError":false,"timestamp":0
        }))
        .unwrap();
        assert_eq!(tool_result.details, Some(Value::Null));
        assert_eq!(
            serde_json::to_value(tool_result).unwrap(),
            json!({
                "role":"toolResult","toolCallId":"c","toolName":"t","content":[],
                "details":null,"isError":false,"timestamp":0
            })
        );
    }

    /// Pins pi `types.ts:225,297-305`: provider option intersections retain custom JSON keys.
    #[test]
    fn provider_option_records_round_trip_extension_keys() {
        let stream_json = json!({
            "temperature":0.0,
            "headers":{"x-default":null},
            "vendorOption":{"enabled":true},
            "vendorNull":null
        });
        let stream: ProviderStreamOptions = serde_json::from_value(stream_json.clone()).unwrap();
        assert_eq!(stream.stream.temperature, Some(0.0));
        assert_eq!(stream.extra.get("vendorNull"), Some(&Value::Null));
        assert_eq!(serde_json::to_value(stream).unwrap(), stream_json);

        let images_json = json!({
            "metadata":{"user":"u"},
            "outputFormat":"png",
            "seed":0
        });
        let images: ProviderImagesOptions = serde_json::from_value(images_json.clone()).unwrap();
        assert_eq!(images.images.metadata.as_ref().unwrap()["user"], "u");
        assert_eq!(images.extra.get("seed"), Some(&json!(0)));
        assert_eq!(serde_json::to_value(images).unwrap(), images_json);
    }

    /// Pins pi `types.ts:558-715,821-850`: compat wire objects are selected by `model.api`.
    #[test]
    fn model_compat_is_api_discriminated_without_wire_tag() {
        let anthropic_json = json!({
            "id":"m","name":"M","api":"anthropic-messages","provider":"anthropic",
            "baseUrl":"https://example.test","reasoning":true,"input":["text"],
            "cost":{"input":1.0,"output":2.0,"cacheRead":0.1,"cacheWrite":0.2},
            "contextWindow":100,"maxTokens":10,
            "compat":{"supportsTemperature":false,"allowEmptySignature":true}
        });
        let model: Model = serde_json::from_value(anthropic_json.clone()).unwrap();
        assert!(matches!(
            model.compat,
            Some(ModelCompat::AnthropicMessages(_))
        ));
        assert_eq!(serde_json::to_value(model).unwrap(), anthropic_json);

        let cases = [
            (
                "openai-completions",
                json!({"supportsStore":false,"supportsOpenAIGrammarTools":true}),
                "completions",
            ),
            (
                "openai-responses",
                json!({"supportsAdditionalTools":true,"supportsExplicitPromptCacheMode":false}),
                "responses",
            ),
            (
                "bedrock-converse-stream",
                json!({"supportsStrictMode":true}),
                "bedrock",
            ),
        ];
        for (api, compat, family) in cases {
            let wire = json!({
                "id":"m","name":"M","api":api,"provider":"custom",
                "baseUrl":"https://example.test","reasoning":false,"input":["text"],
                "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0},
                "contextWindow":100,"maxTokens":10,"compat":compat
            });
            let model: Model = serde_json::from_value(wire.clone()).unwrap();
            assert!(
                matches!(
                    (&model.compat, family),
                    (Some(ModelCompat::OpenAICompletions(_)), "completions")
                        | (Some(ModelCompat::OpenAIResponses(_)), "responses")
                        | (Some(ModelCompat::Bedrock(_)), "bedrock")
                ),
                "wrong compat family for {api}"
            );
            assert_eq!(serde_json::to_value(model).unwrap(), wire);
        }

        let invalid = json!({
            "id":"m","name":"M","api":"custom-api","provider":"custom",
            "baseUrl":"http://localhost","reasoning":false,"input":["text"],
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
            "contextWindow":1,"maxTokens":1,"compat":{}
        });
        assert!(serde_json::from_value::<Model>(invalid).is_err());

        let mismatched = Model {
            id: "m".into(),
            name: "M".into(),
            api: Api::from("anthropic-messages"),
            provider: ProviderId::from("anthropic"),
            base_url: "https://example.test".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 1,
            max_tokens: 1,
            sampling_params: None,
            headers: None,
            compat: Some(ModelCompat::OpenAIResponses(
                OpenAIResponsesCompat::default(),
            )),
        };
        assert!(serde_json::to_value(mismatched).is_err());
    }
}
