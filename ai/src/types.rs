//! Provider-neutral data contracts mirrored from pi `src/types.ts`.

use futures::future::BoxFuture;
use indexmap::IndexMap;
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

fn deserialize_null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

pub(crate) fn serialize_js_f64<S>(value: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if *value == 0.0 {
        serializer.serialize_i64(0)
    } else if value.is_finite()
        && value.fract() == 0.0
        && *value >= i64::MIN as f64
        && *value <= i64::MAX as f64
    {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

pub(crate) fn serialize_optional_js_f64<S>(
    value: &Option<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(value) => serialize_js_f64(value, serializer),
        None => serializer.serialize_none(),
    }
}

pub(crate) fn js_f64_value(value: f64) -> Value {
    if value == 0.0 {
        Value::from(0)
    } else if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64
    {
        Value::from(value as i64)
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBudgets {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_js_f64"
    )]
    pub minimal: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_js_f64"
    )]
    pub low: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_js_f64"
    )]
    pub medium: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_js_f64"
    )]
    pub high: Option<f64>,
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

pub type ProviderEnv = IndexMap<String, String>;
pub type ProviderHeaders = IndexMap<String, Option<String>>;

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

    fn reason(&self) -> Option<crate::utils::abort::AbortReason> {
        None
    }
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

pub type OnPayload<TModel = Model> = Arc<
    dyn for<'a> Fn(Value, &'a TModel) -> BoxFuture<'a, Result<Option<Value>, String>>
        + Send
        + Sync
        + 'static,
>;
pub type OnResponse<TModel = Model> = Arc<
    dyn for<'a> Fn(ProviderResponse, &'a TModel) -> BoxFuture<'a, Result<(), String>>
        + Send
        + Sync
        + 'static,
>;

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
    #[serde(serialize_with = "serialize_optional_js_f64")]
    pub timeout_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "serialize_optional_js_f64")]
    pub max_retries: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "serialize_optional_js_f64")]
    pub max_retry_delay_ms: Option<f64>,
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_js_f64"
    )]
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
    #[serde(serialize_with = "serialize_optional_js_f64")]
    pub websocket_connect_timeout_ms: Option<f64>,
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
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<Value>,
    ) -> Self {
        Self {
            kind: ToolCallType::ToolCall,
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
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

#[derive(Debug, Clone, PartialEq)]
pub enum AssistantMessageContent {
    Null,
    Blocks(Vec<AssistantContent>),
}

impl Default for AssistantMessageContent {
    fn default() -> Self {
        Self::Blocks(Vec::new())
    }
}

impl From<Vec<AssistantContent>> for AssistantMessageContent {
    fn from(value: Vec<AssistantContent>) -> Self {
        Self::Blocks(value)
    }
}

impl std::ops::Deref for AssistantMessageContent {
    type Target = Vec<AssistantContent>;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Null => &EMPTY_ASSISTANT_CONTENT,
            Self::Blocks(blocks) => blocks,
        }
    }
}

static EMPTY_ASSISTANT_CONTENT: Vec<AssistantContent> = Vec::new();

impl std::ops::DerefMut for AssistantMessageContent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        if matches!(self, Self::Null) {
            *self = Self::default();
        }
        match self {
            Self::Blocks(blocks) => blocks,
            Self::Null => unreachable!("null was normalized for mutation"),
        }
    }
}

impl<'a> IntoIterator for &'a AssistantMessageContent {
    type Item = &'a AssistantContent;
    type IntoIter = std::slice::Iter<'a, AssistantContent>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Serialize for AssistantMessageContent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Blocks(blocks) => blocks.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for AssistantMessageContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<Vec<AssistantContent>>::deserialize(deserializer)
            .map(|value| value.map_or(Self::Null, Self::Blocks))
    }
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

impl Default for UserContent {
    fn default() -> Self {
        Self::Blocks(Vec::new())
    }
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
    #[serde(default, deserialize_with = "deserialize_null_as_default")]
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
    #[serde(serialize_with = "serialize_js_f64")]
    pub input: f64,
    #[serde(serialize_with = "serialize_js_f64")]
    pub output: f64,
    #[serde(serialize_with = "serialize_js_f64")]
    pub cache_read: f64,
    #[serde(serialize_with = "serialize_js_f64")]
    pub cache_write: f64,
    #[serde(serialize_with = "serialize_js_f64")]
    pub total: f64,
}

#[derive(Debug, Clone)]
pub struct UsageValue(UsageValueRepr);

#[derive(Debug, Clone)]
enum UsageValueRepr {
    Number(f64),
    Other(Value),
}

impl Default for UsageValue {
    fn default() -> Self {
        Self(UsageValueRepr::Number(0.0))
    }
}

impl PartialEq for UsageValue {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (UsageValueRepr::Number(left), UsageValueRepr::Number(right)) => left == right,
            (UsageValueRepr::Other(left), UsageValueRepr::Other(right)) => left == right,
            _ => false,
        }
    }
}

macro_rules! usage_value_from_number {
    ($($kind:ty),+ $(,)?) => {
        $(
            impl From<$kind> for UsageValue {
                fn from(value: $kind) -> Self {
                    Self(UsageValueRepr::Number(value as f64))
                }
            }

            impl PartialEq<$kind> for UsageValue {
                fn eq(&self, other: &$kind) -> bool {
                    matches!(&self.0, UsageValueRepr::Number(value) if *value == *other as f64)
                }
            }
        )+
    };
}

usage_value_from_number!(i32, i64, u32, u64, usize, f32, f64);

impl From<Value> for UsageValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Number(value) => {
                Self(UsageValueRepr::Number(value.as_f64().unwrap_or(f64::NAN)))
            }
            value => Self(UsageValueRepr::Other(value)),
        }
    }
}

impl Serialize for UsageValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.0 {
            UsageValueRepr::Number(value) => serialize_js_f64(value, serializer),
            UsageValueRepr::Other(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for UsageValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Value::deserialize(deserializer).map(Self::from)
    }
}

impl UsageValue {
    pub fn as_number(&self) -> f64 {
        match &self.0 {
            UsageValueRepr::Number(value) => *value,
            UsageValueRepr::Other(Value::Null) => 0.0,
            UsageValueRepr::Other(Value::Bool(value)) => f64::from(u8::from(*value)),
            UsageValueRepr::Other(Value::String(value)) => javascript_number(value),
            UsageValueRepr::Other(Value::Array(values)) => match values.as_slice() {
                [] => 0.0,
                [value] => UsageValue::from(value.clone()).as_number(),
                _ => f64::NAN,
            },
            UsageValueRepr::Other(Value::Object(_)) => f64::NAN,
            UsageValueRepr::Other(Value::Number(_)) => unreachable!("numbers use Number"),
        }
    }

    pub fn is_truthy(&self) -> bool {
        match &self.0 {
            UsageValueRepr::Number(value) => *value != 0.0 && !value.is_nan(),
            UsageValueRepr::Other(Value::Null) => false,
            UsageValueRepr::Other(Value::Bool(value)) => *value,
            UsageValueRepr::Other(Value::String(value)) => !value.is_empty(),
            UsageValueRepr::Other(Value::Array(_) | Value::Object(_)) => true,
            UsageValueRepr::Other(Value::Number(_)) => unreachable!("numbers use Number"),
        }
    }

    pub(crate) fn js_add(&self, other: &Self) -> Self {
        match (self.string_primitive(), other.string_primitive()) {
            (Some(left), Some(right)) => Value::String(left + &right).into(),
            (Some(left), None) => Value::String(left + &other.non_string_primitive()).into(),
            (None, Some(right)) => Value::String(self.non_string_primitive() + &right).into(),
            (None, None) => (self.as_number() + other.as_number()).into(),
        }
    }

    fn string_primitive(&self) -> Option<String> {
        match &self.0 {
            UsageValueRepr::Other(Value::String(value)) => Some(value.clone()),
            UsageValueRepr::Other(Value::Array(values)) => Some(
                values
                    .iter()
                    .map(|value| match value {
                        Value::Null => String::new(),
                        Value::String(value) => value.clone(),
                        value => UsageValue::from(value.clone()).non_string_primitive(),
                    })
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            UsageValueRepr::Other(Value::Object(_)) => Some("[object Object]".to_owned()),
            _ => None,
        }
    }

    fn non_string_primitive(&self) -> String {
        match &self.0 {
            UsageValueRepr::Number(value) => javascript_number_string(*value),
            UsageValueRepr::Other(Value::Null) => "null".to_owned(),
            UsageValueRepr::Other(Value::Bool(value)) => value.to_string(),
            UsageValueRepr::Other(Value::String(value)) => value.clone(),
            UsageValueRepr::Other(Value::Array(_) | Value::Object(_)) => {
                self.string_primitive().unwrap_or_default()
            }
            UsageValueRepr::Other(Value::Number(_)) => unreachable!("numbers use Number"),
        }
    }
}

fn javascript_number(value: &str) -> f64 {
    let value = crate::utils::error_body::trim_javascript_whitespace(value);
    if value.is_empty() {
        return 0.0;
    }
    match value {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefixes, radix) in [(["0x", "0X"], 16), (["0b", "0B"], 2), (["0o", "0O"], 8)] {
        if let Some(digits) = value
            .strip_prefix(prefixes[0])
            .or_else(|| value.strip_prefix(prefixes[1]))
        {
            return u128::from_str_radix(digits, radix).map_or(f64::NAN, |number| number as f64);
        }
    }
    value
        .parse()
        .ok()
        .filter(|number: &f64| number.is_finite())
        .unwrap_or(f64::NAN)
}

fn javascript_number_string(value: f64) -> String {
    crate::utils::error_body::js_f64_string(value)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: UsageValue,
    pub output: UsageValue,
    pub cache_read: UsageValue,
    pub cache_write: UsageValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<UsageValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<UsageValue>,
    pub total_tokens: UsageValue,
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
    #[serde(default)]
    pub content: AssistantMessageContent,
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
            content: AssistantMessageContent::default(),
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
    #[serde(default, deserialize_with = "deserialize_null_as_default")]
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
    #[serde(serialize_with = "serialize_js_f64")]
    pub input: f64,
    #[serde(serialize_with = "serialize_js_f64")]
    pub output: f64,
    #[serde(serialize_with = "serialize_js_f64")]
    pub cache_read: f64,
    #[serde(serialize_with = "serialize_js_f64")]
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum NumberOrString {
    Number(f64),
    String(String),
}

impl Serialize for NumberOrString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Number(value) => serialize_js_f64(value, serializer),
            Self::String(value) => serializer.serialize_str(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoutingSort {
    Name(String),
    Options(RoutingSortOptions),
}

fn serialize_ordered_object<S>(
    mut fields: Map<String, Value>,
    field_order: &[String],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut ordered = Map::new();
    for key in field_order {
        if let Some(value) = fields.remove(key) {
            ordered.insert(key.clone(), value);
        }
    }
    ordered.extend(fields);
    ordered.serialize(serializer)
}

fn deserialize_field<T>(fields: &mut Map<String, Value>, name: &str) -> Result<Option<T>, String>
where
    T: for<'de> Deserialize<'de>,
{
    fields
        .remove(name)
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| error.to_string())
}

macro_rules! insert_optional_field {
    ($fields:expr, $name:literal, $value:expr) => {
        if let Some(value) = $value {
            $fields.insert(
                $name.to_owned(),
                serde_json::to_value(value).map_err(S::Error::custom)?,
            );
        }
    };
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoutingSortOptions {
    pub by: Option<String>,
    pub partition: Option<Option<String>>,
    pub extra: Map<String, Value>,
    #[doc(hidden)]
    pub __field_order: Vec<String>,
}

impl Serialize for RoutingSortOptions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut fields = self.extra.clone();
        insert_optional_field!(fields, "by", self.by.as_ref());
        if let Some(partition) = &self.partition {
            fields.insert(
                "partition".to_owned(),
                partition.clone().map_or(Value::Null, Value::String),
            );
        }
        serialize_ordered_object(fields, &self.__field_order, serializer)
    }
}

impl<'de> Deserialize<'de> for RoutingSortOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut fields = Map::<String, Value>::deserialize(deserializer)?;
        let field_order = fields.keys().cloned().collect();
        let by = deserialize_field(&mut fields, "by").map_err(D::Error::custom)?;
        let partition = fields
            .remove("partition")
            .map(|value| {
                if value.is_null() {
                    Ok(None)
                } else {
                    serde_json::from_value(value)
                        .map(Some)
                        .map_err(D::Error::custom)
                }
            })
            .transpose()?;
        Ok(Self {
            by,
            partition,
            extra: fields,
            __field_order: field_order,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenRouterMaxPrice {
    pub prompt: Option<NumberOrString>,
    pub completion: Option<NumberOrString>,
    pub image: Option<NumberOrString>,
    pub audio: Option<NumberOrString>,
    pub request: Option<NumberOrString>,
    pub extra: Map<String, Value>,
    #[doc(hidden)]
    pub __field_order: Vec<String>,
}

impl Serialize for OpenRouterMaxPrice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut fields = self.extra.clone();
        insert_optional_field!(fields, "prompt", self.prompt.as_ref());
        insert_optional_field!(fields, "completion", self.completion.as_ref());
        insert_optional_field!(fields, "image", self.image.as_ref());
        insert_optional_field!(fields, "audio", self.audio.as_ref());
        insert_optional_field!(fields, "request", self.request.as_ref());
        serialize_ordered_object(fields, &self.__field_order, serializer)
    }
}

impl<'de> Deserialize<'de> for OpenRouterMaxPrice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut fields = Map::<String, Value>::deserialize(deserializer)?;
        let field_order = fields.keys().cloned().collect();
        Ok(Self {
            prompt: deserialize_field(&mut fields, "prompt").map_err(D::Error::custom)?,
            completion: deserialize_field(&mut fields, "completion").map_err(D::Error::custom)?,
            image: deserialize_field(&mut fields, "image").map_err(D::Error::custom)?,
            audio: deserialize_field(&mut fields, "audio").map_err(D::Error::custom)?,
            request: deserialize_field(&mut fields, "request").map_err(D::Error::custom)?,
            extra: fields,
            __field_order: field_order,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PercentileThresholds {
    pub p50: Option<f64>,
    pub p75: Option<f64>,
    pub p90: Option<f64>,
    pub p99: Option<f64>,
    pub extra: Map<String, Value>,
    #[doc(hidden)]
    pub __field_order: Vec<String>,
}

impl Serialize for PercentileThresholds {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut fields = self.extra.clone();
        for (name, value) in [
            ("p50", self.p50),
            ("p75", self.p75),
            ("p90", self.p90),
            ("p99", self.p99),
        ] {
            if let Some(value) = value {
                fields.insert(name.to_owned(), js_f64_value(value));
            }
        }
        serialize_ordered_object(fields, &self.__field_order, serializer)
    }
}

impl<'de> Deserialize<'de> for PercentileThresholds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut fields = Map::<String, Value>::deserialize(deserializer)?;
        let field_order = fields.keys().cloned().collect();
        Ok(Self {
            p50: deserialize_field(&mut fields, "p50").map_err(D::Error::custom)?,
            p75: deserialize_field(&mut fields, "p75").map_err(D::Error::custom)?,
            p90: deserialize_field(&mut fields, "p90").map_err(D::Error::custom)?,
            p99: deserialize_field(&mut fields, "p99").map_err(D::Error::custom)?,
            extra: fields,
            __field_order: field_order,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum RoutingThreshold {
    Number(f64),
    Percentiles(PercentileThresholds),
}

impl Serialize for RoutingThreshold {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Number(value) => serialize_js_f64(value, serializer),
            Self::Percentiles(value) => value.serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenRouterRouting {
    pub allow_fallbacks: Option<bool>,
    pub require_parameters: Option<bool>,
    pub data_collection: Option<DataCollection>,
    pub zdr: Option<bool>,
    pub enforce_distillable_text: Option<bool>,
    pub order: Option<Vec<String>>,
    pub only: Option<Vec<String>>,
    pub ignore: Option<Vec<String>>,
    pub quantizations: Option<Vec<String>>,
    pub sort: Option<RoutingSort>,
    pub max_price: Option<OpenRouterMaxPrice>,
    pub preferred_min_throughput: Option<RoutingThreshold>,
    pub preferred_max_latency: Option<RoutingThreshold>,
    pub extra: Map<String, Value>,
    #[doc(hidden)]
    pub __field_order: Vec<String>,
}

impl Serialize for OpenRouterRouting {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut fields = self.extra.clone();
        insert_optional_field!(fields, "allow_fallbacks", self.allow_fallbacks);
        insert_optional_field!(fields, "require_parameters", self.require_parameters);
        insert_optional_field!(fields, "data_collection", self.data_collection);
        insert_optional_field!(fields, "zdr", self.zdr);
        insert_optional_field!(
            fields,
            "enforce_distillable_text",
            self.enforce_distillable_text
        );
        insert_optional_field!(fields, "order", self.order.as_ref());
        insert_optional_field!(fields, "only", self.only.as_ref());
        insert_optional_field!(fields, "ignore", self.ignore.as_ref());
        insert_optional_field!(fields, "quantizations", self.quantizations.as_ref());
        insert_optional_field!(fields, "sort", self.sort.as_ref());
        insert_optional_field!(fields, "max_price", self.max_price.as_ref());
        insert_optional_field!(
            fields,
            "preferred_min_throughput",
            self.preferred_min_throughput.as_ref()
        );
        insert_optional_field!(
            fields,
            "preferred_max_latency",
            self.preferred_max_latency.as_ref()
        );
        serialize_ordered_object(fields, &self.__field_order, serializer)
    }
}

impl<'de> Deserialize<'de> for OpenRouterRouting {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut fields = Map::<String, Value>::deserialize(deserializer)?;
        let field_order = fields.keys().cloned().collect();
        Ok(Self {
            allow_fallbacks: deserialize_field(&mut fields, "allow_fallbacks")
                .map_err(D::Error::custom)?,
            require_parameters: deserialize_field(&mut fields, "require_parameters")
                .map_err(D::Error::custom)?,
            data_collection: deserialize_field(&mut fields, "data_collection")
                .map_err(D::Error::custom)?,
            zdr: deserialize_field(&mut fields, "zdr").map_err(D::Error::custom)?,
            enforce_distillable_text: deserialize_field(&mut fields, "enforce_distillable_text")
                .map_err(D::Error::custom)?,
            order: deserialize_field(&mut fields, "order").map_err(D::Error::custom)?,
            only: deserialize_field(&mut fields, "only").map_err(D::Error::custom)?,
            ignore: deserialize_field(&mut fields, "ignore").map_err(D::Error::custom)?,
            quantizations: deserialize_field(&mut fields, "quantizations")
                .map_err(D::Error::custom)?,
            sort: deserialize_field(&mut fields, "sort").map_err(D::Error::custom)?,
            max_price: deserialize_field(&mut fields, "max_price").map_err(D::Error::custom)?,
            preferred_min_throughput: deserialize_field(&mut fields, "preferred_min_throughput")
                .map_err(D::Error::custom)?,
            preferred_max_latency: deserialize_field(&mut fields, "preferred_max_latency")
                .map_err(D::Error::custom)?,
            extra: fields,
            __field_order: field_order,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataCollection {
    Deny,
    Allow,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VercelGatewayRouting {
    pub only: Option<Vec<String>>,
    pub order: Option<Vec<String>>,
    pub extra: Map<String, Value>,
    #[doc(hidden)]
    pub __field_order: Vec<String>,
}

impl Serialize for VercelGatewayRouting {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut fields = self.extra.clone();
        insert_optional_field!(fields, "only", self.only.as_ref());
        insert_optional_field!(fields, "order", self.order.as_ref());
        serialize_ordered_object(fields, &self.__field_order, serializer)
    }
}

impl<'de> Deserialize<'de> for VercelGatewayRouting {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut fields = Map::<String, Value>::deserialize(deserializer)?;
        let field_order = fields.keys().cloned().collect();
        Ok(Self {
            only: deserialize_field(&mut fields, "only").map_err(D::Error::custom)?,
            order: deserialize_field(&mut fields, "order").map_err(D::Error::custom)?,
            extra: fields,
            __field_order: field_order,
        })
    }
}

macro_rules! ordered_compat_object {
    (
        pub struct $name:ident {
            $(pub $field:ident: $ty:ty => $wire_name:literal,)*
        }
    ) => {
        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct $name {
            $(pub $field: Option<$ty>,)*
            pub extra: Map<String, Value>,
            #[doc(hidden)]
            pub __field_order: Vec<String>,
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut fields = self.extra.clone();
                $(insert_optional_field!(fields, $wire_name, self.$field.as_ref());)*
                serialize_ordered_object(fields, &self.__field_order, serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let mut fields = Map::<String, Value>::deserialize(deserializer)?;
                let field_order = fields.keys().cloned().collect();
                Ok(Self {
                    $($field: deserialize_field(&mut fields, $wire_name)
                        .map_err(D::Error::custom)?,)*
                    extra: fields,
                    __field_order: field_order,
                })
            }
        }
    };
}

ordered_compat_object! {
    pub struct OpenAICompletionsCompat {
        pub supports_store: bool => "supportsStore",
        pub supports_developer_role: bool => "supportsDeveloperRole",
        pub supports_reasoning_effort: bool => "supportsReasoningEffort",
        pub supports_usage_in_streaming: bool => "supportsUsageInStreaming",
        pub supports_finish_reason: bool => "supportsFinishReason",
        pub max_tokens_field: MaxTokensField => "maxTokensField",
        pub requires_tool_result_name: bool => "requiresToolResultName",
        pub requires_assistant_after_tool_result: bool => "requiresAssistantAfterToolResult",
        pub requires_thinking_as_text: bool => "requiresThinkingAsText",
        pub requires_reasoning_content_on_assistant_messages: bool => "requiresReasoningContentOnAssistantMessages",
        pub thinking_format: ThinkingFormat => "thinkingFormat",
        pub chat_template_kwargs: BTreeMap<String, ChatTemplateKwargValue> => "chatTemplateKwargs",
        pub chat_template_args: BTreeMap<String, ChatTemplateKwargValue> => "chatTemplateArgs",
        pub open_router_routing: OpenRouterRouting => "openRouterRouting",
        pub vercel_gateway_routing: VercelGatewayRouting => "vercelGatewayRouting",
        pub zai_tool_stream: bool => "zaiToolStream",
        pub thinking_token_budget_field: ThinkingTokenBudgetField => "thinkingTokenBudgetField",
        pub supports_thinking_token_budget: bool => "supportsThinkingTokenBudget",
        pub supports_open_ai_grammar_tools: bool => "supportsOpenAIGrammarTools",
        pub supports_strict_mode: bool => "supportsStrictMode",
        pub cache_control_format: CacheControlFormat => "cacheControlFormat",
        pub send_session_affinity_headers: bool => "sendSessionAffinityHeaders",
        pub deferred_tools_mode: DeferredToolsMode => "deferredToolsMode",
        pub session_affinity_format: SessionAffinityFormat => "sessionAffinityFormat",
        pub supports_long_cache_retention: bool => "supportsLongCacheRetention",
    }
}

ordered_compat_object! {
    pub struct OpenAIResponsesCompat {
        pub supports_developer_role: bool => "supportsDeveloperRole",
        pub session_affinity_format: SessionAffinityFormat => "sessionAffinityFormat",
        pub supports_long_cache_retention: bool => "supportsLongCacheRetention",
        pub supports_strict_mode: bool => "supportsStrictMode",
        pub supports_open_ai_grammar_tools: bool => "supportsOpenAIGrammarTools",
        pub supports_additional_tools: bool => "supportsAdditionalTools",
        pub supports_tool_search: bool => "supportsToolSearch",
        pub supports_explicit_prompt_cache_mode: bool => "supportsExplicitPromptCacheMode",
    }
}

ordered_compat_object! {
    pub struct AnthropicMessagesCompat {
        pub supports_eager_tool_input_streaming: bool => "supportsEagerToolInputStreaming",
        pub supports_long_cache_retention: bool => "supportsLongCacheRetention",
        pub send_session_affinity_headers: bool => "sendSessionAffinityHeaders",
        pub supports_cache_control_on_tools: bool => "supportsCacheControlOnTools",
        pub supports_temperature: bool => "supportsTemperature",
        pub force_adaptive_thinking: bool => "forceAdaptiveThinking",
        pub allow_empty_signature: bool => "allowEmptySignature",
        pub supports_strict_tools: bool => "supportsStrictTools",
        pub allowed_fallback_models: Vec<AnthropicAllowedFallbackModel> => "allowedFallbackModels",
        pub supports_tool_references: bool => "supportsToolReferences",
    }
}

ordered_compat_object! {
    pub struct BedrockCompat {
        pub supports_strict_mode: bool => "supportsStrictMode",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ModelCompat {
    OpenAICompletions(Box<OpenAICompletionsCompat>),
    OpenAIResponses(OpenAIResponsesCompat),
    AnthropicMessages(AnthropicMessagesCompat),
    Bedrock(BedrockCompat),
    Custom(Value),
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
    pub headers: Option<IndexMap<String, String>>,
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
    headers: &'a Option<IndexMap<String, String>>,
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
    headers: Option<IndexMap<String, String>>,
    #[serde(default)]
    compat: Option<Value>,
}

impl Serialize for Model {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
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
        let compat = wire.compat.map(ModelCompat::Custom);
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
            input: 11.into(),
            output: 7.into(),
            cache_read: 3.into(),
            cache_write: 5.into(),
            cache_write_1h: Some(0.into()),
            reasoning: Some(0.into()),
            total_tokens: 18.into(),
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
                "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}
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
            ]
            .into(),
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
                "totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
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
            headers: Some(IndexMap::from([
                ("x-default".into(), None),
                ("x-empty".into(), Some(String::new())),
            ])),
            max_retry_delay_ms: Some(0.0),
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

    /// Pins pi `types.ts:100-105,163-177,382-404` JavaScript-number fields.
    #[test]
    fn request_budgets_and_usage_accept_signed_fractional_numbers() {
        let options: ProviderRequestOptions<Model> = serde_json::from_value(json!({
            "timeoutMs":-1.5,
            "maxRetries":0.5,
            "maxRetryDelayMs":-2.25
        }))
        .unwrap();
        assert_eq!(options.timeout_ms, Some(-1.5));
        assert_eq!(options.max_retries, Some(0.5));
        assert_eq!(options.max_retry_delay_ms, Some(-2.25));

        let budgets: ThinkingBudgets = serde_json::from_value(json!({
            "minimal":-1.25,
            "high":8192.5
        }))
        .unwrap();
        assert_eq!(budgets.minimal, Some(-1.25));
        assert_eq!(budgets.high, Some(8192.5));

        let usage: Usage = serde_json::from_value(json!({
            "input":-1.25,
            "output":0.5,
            "cacheRead":"2",
            "cacheWrite":0,
            "reasoning":-0.5,
            "totalTokens":"1.250",
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}
        }))
        .unwrap();
        let wire = serde_json::to_value(usage).unwrap();
        assert_eq!(wire["input"], -1.25);
        assert_eq!(wire["output"], 0.5);
        assert_eq!(wire["cacheRead"], "2");
        assert_eq!(wire["reasoning"], -0.5);
        assert_eq!(wire["totalTokens"], "1.250");
        assert!(UsageValue::from(json!("inf")).as_number().is_nan());
        assert_eq!(
            UsageValue::from(json!("Infinity")).as_number(),
            f64::INFINITY
        );
        assert_eq!(UsageValue::from(json!("\u{feff}1")).as_number(), 1.0);
        assert!(UsageValue::from(json!("\u{0085}1")).as_number().is_nan());
    }

    /// Pins pi `types.ts:225,297-305`: provider option intersections retain custom JSON keys.
    #[test]
    fn provider_option_records_round_trip_extension_keys() {
        let stream_json = json!({
            "temperature":0,
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

    /// Pins pi `types.ts:821-850`: compat is runtime JSON and preserves original key order.
    #[test]
    fn model_compat_is_api_discriminated_without_wire_tag() {
        let anthropic_json = json!({
            "id":"m","name":"M","api":"anthropic-messages","provider":"anthropic",
            "baseUrl":"https://example.test","reasoning":true,"input":["text"],
            "cost":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":0.2},
            "contextWindow":100,"maxTokens":10,
            "compat":{"supportsTemperature":false,"allowEmptySignature":true}
        });
        let model: Model = serde_json::from_value(anthropic_json.clone()).unwrap();
        assert!(matches!(model.compat, Some(ModelCompat::Custom(_))));
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
        for (api, compat, _family) in cases {
            let wire = json!({
                "id":"m","name":"M","api":api,"provider":"custom",
                "baseUrl":"https://example.test","reasoning":false,"input":["text"],
                "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
                "contextWindow":100,"maxTokens":10,"compat":compat
            });
            let model: Model = serde_json::from_value(wire.clone()).unwrap();
            assert!(
                matches!(&model.compat, Some(ModelCompat::Custom(value)) if value == &wire["compat"])
            );
            assert_eq!(serde_json::to_value(model).unwrap(), wire);
        }
    }

    /// Pins pi `types.ts:742-750`: routing sort objects are schema-free runtime
    /// data, so unknown keys survive load and serialization.
    #[test]
    fn routing_sort_options_preserve_unknown_keys() {
        let wire = json!({"before":1,"by":"latency","middle":2,"partition":null,"after":3});
        let decoded: RoutingSortOptions = serde_json::from_value(wire.clone()).unwrap();
        assert_round_trip(&decoded, wire);
    }

    /// Pins pi `types.ts:751-763`: max-price objects are schema-free runtime
    /// data, including custom nested price dimensions.
    #[test]
    fn open_router_max_price_preserves_unknown_keys() {
        let wire = json!({"before":0,"prompt":1,"middle":2,"completion":"2.5","video":{"unit":"second","price":3}});
        let decoded: OpenRouterMaxPrice = serde_json::from_value(wire.clone()).unwrap();
        assert_round_trip(&decoded, wire);
    }

    /// Pins pi `types.ts:764-789`: percentile threshold objects retain unknown
    /// percentile keys exactly as loaded.
    #[test]
    fn percentile_thresholds_preserve_unknown_keys() {
        let wire = json!({"before":5,"p50":10,"middle":15,"p99":20,"p99_9":25});
        let decoded: PercentileThresholds = serde_json::from_value(wire.clone()).unwrap();
        assert_round_trip(&decoded, wire);
    }

    /// Pins pi `types.ts:717-790`: OpenRouter routing is schema-free runtime
    /// data, so custom routing fields survive load and serialization.
    #[test]
    fn open_router_routing_preserves_unknown_keys() {
        let wire = json!({
            "before": 1,
            "only":["provider-a"],
            "middle": 2,
            "order":["provider-b"],
            "custom_router":{"region":"west","weights":[1,2]}
        });
        let decoded: OpenRouterRouting = serde_json::from_value(wire.clone()).unwrap();
        assert_round_trip(&decoded, wire);
    }

    /// Pins pi `types.ts:792-802`: Vercel gateway routing is schema-free runtime
    /// data, so future gateway keys round-trip unchanged.
    #[test]
    fn vercel_gateway_routing_preserves_unknown_keys() {
        let wire = json!({"before":1,"order":["anthropic"],"middle":2,"only":["openai"],"after":3});
        let decoded: VercelGatewayRouting = serde_json::from_value(wire.clone()).unwrap();
        assert_round_trip(&decoded, wire);
    }

    /// Pins pi `types.ts:822-850`: known compat families remain schema-free
    /// runtime objects, including unknown keys and their JSON values.
    #[test]
    fn known_api_compat_preserves_unknown_keys_as_inert_data() {
        let cases = [
            (
                "openai-completions",
                json!({
                    "beforeCompletionsFlag": 1,
                    "supportsStore": false,
                    "futureCompletionsFlag": {"mode":"new","value":1}
                }),
            ),
            (
                "openai-responses",
                json!({
                    "beforeResponsesFlag": 1,
                    "supportsDeveloperRole": true,
                    "futureResponsesFlag": [1, null, "x"]
                }),
            ),
            (
                "anthropic-messages",
                json!({
                    "beforeAnthropicFlag": 1,
                    "supportsTemperature": false,
                    "futureAnthropicFlag": 0
                }),
            ),
            (
                "bedrock-converse-stream",
                json!({
                    "beforeBedrockFlag": 1,
                    "supportsStrictMode": true,
                    "futureBedrockFlag": null
                }),
            ),
        ];

        for (api, compat) in cases {
            let wire = json!({
                "id":"m","name":"M","api":api,"provider":"custom",
                "baseUrl":"https://example.test","reasoning":false,"input":["text"],
                "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
                "contextWindow":100,"maxTokens":10,"compat":compat
            });
            let model: Model = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(
                serde_json::to_string(&model).unwrap(),
                serde_json::to_string(&wire).unwrap(),
                "{api} bytes"
            );
            assert_eq!(serde_json::to_value(model).unwrap(), wire, "{api}");
        }
    }

    /// Pins pi `types.ts:822-850`: the compat family constraint is compile-time-only.
    #[test]
    fn custom_api_compat_loads_and_round_trips_as_inert_data() {
        let wire = json!({
            "id":"m","name":"M","api":"custom-api","provider":"custom",
            "baseUrl":"http://localhost","reasoning":false,"input":["text"],
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
            "contextWindow":1,"maxTokens":1,
            "compat":{"family":"opaque","supportsSomething":true,"nested":{"value":1}}
        });
        let model: Model = serde_json::from_value(wire.clone()).expect("pi accepts inert compat");
        assert!(matches!(
            &model.compat,
            Some(ModelCompat::Custom(value)) if value == &wire["compat"]
        ));
        assert_eq!(serde_json::to_value(model).unwrap(), wire);
    }

    /// Pins pi `types.ts:822-850`: runtime serialization does not enforce the TS conditional type.
    #[test]
    fn model_serialization_does_not_runtime_reject_a_compat_shape() {
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
        assert_eq!(
            serde_json::to_value(mismatched).unwrap()["compat"],
            json!({})
        );
    }

    /// Pins pi `src/api/openai-completions.ts:798-800`, `src/api/openai-responses.ts:304-306`,
    /// and `src/api/openai-codex-responses.ts:283,562-564` JSON number formatting.
    #[test]
    fn whole_f64_fields_serialize_like_json_stringify() {
        let options = StreamOptions {
            temperature: Some(1.0),
            ..StreamOptions::default()
        };
        assert_eq!(
            serde_json::to_string(&options).unwrap(),
            r#"{"temperature":1}"#
        );
        assert_eq!(
            serde_json::to_string(&UsageCost {
                input: 1.0,
                output: -0.0,
                cache_read: 0.5,
                cache_write: 2.0,
                total: 3.5,
            })
            .unwrap(),
            r#"{"input":1,"output":0,"cacheRead":0.5,"cacheWrite":2,"total":3.5}"#
        );
        assert_eq!(
            serde_json::to_string(&NumberOrString::Number(4.0)).unwrap(),
            "4"
        );
        assert_eq!(
            serde_json::to_string(&RoutingThreshold::Percentiles(PercentileThresholds {
                p50: Some(1.0),
                p75: None,
                p90: Some(2.5),
                p99: None,
                extra: Map::new(),
                __field_order: Vec::new(),
            }))
            .unwrap(),
            r#"{"p50":1,"p90":2.5}"#
        );
    }
}
