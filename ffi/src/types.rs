//! UniFFI mirrors of the `rust-genai-agent` / `genai-agentprism` data types.
//!
//! Hand-written and compiler-checked: every `From` impl is type-checked against
//! the core types, so upstream changes break this build instead of drifting
//! silently. B1 scope notes (see docs/embedding.md §7):
//!
//! - The message/event tree crosses **outbound only** (events + state
//!   snapshot), so only `From<core> for Mirror` exists for those types.
//! - Inbound config (AgentSetup + the small enums) converts field-wise.
//! - `serde_json::Value` crosses as JSON text in `*_json` fields (§8 naming
//!   rule). Outbound conversion is infallible; inbound JSON
//!   (`initial_messages_json`) is parsed in `AgentSetup::try_into_core`,
//!   which surfaces malformed JSON as a thrown error.
//! - `#[non_exhaustive]` core types (`ThinkingBudgets`) are built via
//!   `Default` + field assignment; non_exhaustive enums get a wildcard arm.

use std::sync::Arc;

use rust_genai_agent as agent;

// region:    --- Error

/// Flat FFI error for the agent surface. `AgentError` is `#[non_exhaustive]`
/// upstream, so unknown future variants land in `Other`.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum AgentError {
    #[error("agent is busy: {0}")]
    Busy(String),
    #[error("no messages to continue from")]
    EmptyContext,
    #[error("cannot continue from message role: assistant")]
    ContinueFromAssistant,
    #[error("no default stream function configured")]
    NoDefaultStreamFn,
    #[error("{0}")]
    Other(String),
}

impl From<agent::AgentError> for AgentError {
    fn from(err: agent::AgentError) -> Self {
        match err {
            agent::AgentError::Busy(ctx) => Self::Busy(format!("{ctx:?}")),
            agent::AgentError::EmptyContext => Self::EmptyContext,
            agent::AgentError::ContinueFromAssistant => Self::ContinueFromAssistant,
            agent::AgentError::NoDefaultStreamFn => Self::NoDefaultStreamFn,
            other => Self::Other(other.to_string()),
        }
    }
}

pub(crate) fn join_err(err: tokio::task::JoinError) -> AgentError {
    AgentError::Other(format!("tokio join error: {err}"))
}

// endregion: --- Error

// region:    --- Config enums & setup (inbound)

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    /// Explicit token budget.
    Budget(u32),
}

impl From<ThinkingLevel> for agent::ThinkingLevel {
    fn from(v: ThinkingLevel) -> Self {
        match v {
            ThinkingLevel::Off => Self::Off,
            ThinkingLevel::Minimal => Self::Minimal,
            ThinkingLevel::Low => Self::Low,
            ThinkingLevel::Medium => Self::Medium,
            ThinkingLevel::High => Self::High,
            ThinkingLevel::XHigh => Self::XHigh,
            ThinkingLevel::Max => Self::Max,
            ThinkingLevel::Budget(tokens) => Self::Budget(tokens),
        }
    }
}

impl From<agent::ThinkingLevel> for ThinkingLevel {
    fn from(v: agent::ThinkingLevel) -> Self {
        match v {
            agent::ThinkingLevel::Off => Self::Off,
            agent::ThinkingLevel::Minimal => Self::Minimal,
            agent::ThinkingLevel::Low => Self::Low,
            agent::ThinkingLevel::Medium => Self::Medium,
            agent::ThinkingLevel::High => Self::High,
            agent::ThinkingLevel::XHigh => Self::XHigh,
            agent::ThinkingLevel::Max => Self::Max,
            agent::ThinkingLevel::Budget(tokens) => Self::Budget(tokens),
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ToolExecutionMode {
    Sequential,
    Parallel,
}

impl From<ToolExecutionMode> for agent::ToolExecutionMode {
    fn from(v: ToolExecutionMode) -> Self {
        match v {
            ToolExecutionMode::Sequential => Self::Sequential,
            ToolExecutionMode::Parallel => Self::Parallel,
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum QueueMode {
    All,
    OneAtATime,
}

impl From<QueueMode> for agent::QueueMode {
    fn from(v: QueueMode) -> Self {
        match v {
            QueueMode::All => Self::All,
            QueueMode::OneAtATime => Self::OneAtATime,
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum Transport {
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}

impl From<Transport> for agent::Transport {
    fn from(v: Transport) -> Self {
        match v {
            Transport::Sse => Self::Sse,
            Transport::Websocket => Self::Websocket,
            Transport::WebsocketCached => Self::WebsocketCached,
            Transport::Auto => Self::Auto,
        }
    }
}

/// Per-level reasoning token budgets.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct ThinkingBudgets {
    pub minimal: Option<u32>,
    pub low: Option<u32>,
    pub medium: Option<u32>,
    pub high: Option<u32>,
}

impl From<ThinkingBudgets> for agent::ThinkingBudgets {
    fn from(v: ThinkingBudgets) -> Self {
        // Core type is #[non_exhaustive]: Default + assignment.
        let mut budgets = Self::default();
        budgets.minimal = v.minimal;
        budgets.low = v.low;
        budgets.medium = v.medium;
        budgets.high = v.high;
        budgets
    }
}

/// Data-only agent configuration — the host constructs this declaratively.
/// Fully typed: the initial transcript and provider chat options included.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AgentSetup {
    pub system_prompt: String,
    pub model: String,
    pub session_id: Option<String>,
    pub messages: Vec<AgentMessage>,
    pub thinking_level: ThinkingLevel,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub tool_execution: ToolExecutionMode,
    pub transport: Transport,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub chat_options: ChatOptions,
}

impl From<AgentSetup> for agent::AgentSetup {
    fn from(v: AgentSetup) -> Self {
        Self {
            system_prompt: v.system_prompt,
            model: v.model,
            session_id: v.session_id,
            messages: v.messages.into_iter().map(Into::into).collect(),
            thinking_level: v.thinking_level.into(),
            thinking_budgets: v.thinking_budgets.map(Into::into),
            max_retries: v.max_retries,
            max_retry_delay_ms: v.max_retry_delay_ms,
            tool_execution: v.tool_execution.into(),
            transport: v.transport.into(),
            steering_mode: v.steering_mode.into(),
            follow_up_mode: v.follow_up_mode.into(),
            chat_options: v.chat_options.into(),
        }
    }
}

// endregion: --- Config enums & setup

// region:    --- Usage & model identity (outbound)

/// Provider adapter + resolved model name, as plain strings.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ModelIden {
    pub adapter_kind: String,
    pub model_name: String,
}

impl From<genai::ModelIden> for ModelIden {
    fn from(v: genai::ModelIden) -> Self {
        Self {
            adapter_kind: v.adapter_kind.to_string(),
            model_name: v.model_name.to_string(),
        }
    }
}

/// Dollar amounts (see core `AgentCost`).
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct AgentCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

impl From<agent::AgentCost> for AgentCost {
    fn from(v: agent::AgentCost) -> Self {
        Self {
            input: v.input,
            output: v.output,
            cache_read: v.cache_read,
            cache_write: v.cache_write,
            total: v.total,
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct AgentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: Option<u64>,
    pub cost: Option<AgentCost>,
}

impl From<agent::AgentUsage> for AgentUsage {
    fn from(v: agent::AgentUsage) -> Self {
        Self {
            input_tokens: v.input_tokens,
            output_tokens: v.output_tokens,
            cache_read_tokens: v.cache_read_tokens,
            cache_write_tokens: v.cache_write_tokens,
            cache_write_1h_tokens: v.cache_write_1h_tokens,
            cost: v.cost.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum StopReason {
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

impl From<agent::StopReason> for StopReason {
    fn from(v: agent::StopReason) -> Self {
        match v {
            agent::StopReason::Pending => Self::Pending,
            agent::StopReason::Stop => Self::Stop,
            agent::StopReason::Length => Self::Length,
            agent::StopReason::ToolUse => Self::ToolUse,
            agent::StopReason::Error => Self::Error,
            agent::StopReason::Aborted => Self::Aborted,
        }
    }
}

// endregion: --- Usage & model identity

// region:    --- Message tree (outbound)

#[derive(Debug, Clone, uniffi::Enum)]
pub enum UserContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime_type: String,
        name: Option<String>,
    },
}

impl From<agent::UserContent> for UserContent {
    fn from(v: agent::UserContent) -> Self {
        match v {
            agent::UserContent::Text { text } => Self::Text { text },
            agent::UserContent::Image {
                data,
                mime_type,
                name,
            } => Self::Image {
                data,
                mime_type,
                name,
            },
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct UserMessage {
    pub content: Vec<UserContent>,
    pub timestamp: i64,
}

impl From<agent::UserMessage> for UserMessage {
    fn from(v: agent::UserMessage) -> Self {
        Self {
            content: v.content.into_iter().map(Into::into).collect(),
            timestamp: v.timestamp,
        }
    }
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ToolResultContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime_type: String,
        name: Option<String>,
    },
}

impl From<agent::ToolResultContent> for ToolResultContent {
    fn from(v: agent::ToolResultContent) -> Self {
        match v {
            agent::ToolResultContent::Text { text } => Self::Text { text },
            agent::ToolResultContent::Image {
                data,
                mime_type,
                name,
            } => Self::Image {
                data,
                mime_type,
                name,
            },
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ToolResultContent>,
    /// JSON text (core `details: Value`).
    pub details_json: String,
    pub usage: Option<AgentUsage>,
    pub added_tool_names: Vec<String>,
    pub is_error: bool,
    pub timestamp: i64,
}

impl From<agent::ToolResultMessage> for ToolResultMessage {
    fn from(v: agent::ToolResultMessage) -> Self {
        Self {
            tool_call_id: v.tool_call_id,
            tool_name: v.tool_name,
            content: v.content.into_iter().map(Into::into).collect(),
            details_json: serde_json::to_string(&v.details).unwrap_or_default(),
            usage: v.usage.map(Into::into),
            added_tool_names: v.added_tool_names,
            is_error: v.is_error,
            timestamp: v.timestamp,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct CustomMessage {
    pub role: String,
    /// JSON text (core `data: Value`).
    pub data_json: String,
    pub timestamp: Option<i64>,
}

impl From<agent::CustomMessage> for CustomMessage {
    fn from(v: agent::CustomMessage) -> Self {
        Self {
            role: v.role,
            data_json: serde_json::to_string(&v.data).unwrap_or_default(),
            timestamp: v.timestamp,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AgentToolCall {
    pub id: String,
    pub name: String,
    /// JSON text (core `arguments: Value`).
    pub arguments_json: String,
    pub namespace: Option<String>,
    pub thought_signatures: Vec<String>,
}

impl From<agent::AgentToolCall> for AgentToolCall {
    fn from(v: agent::AgentToolCall) -> Self {
        Self {
            id: v.id,
            name: v.name,
            arguments_json: serde_json::to_string(&v.arguments).unwrap_or_default(),
            namespace: v.namespace,
            thought_signatures: v.thought_signatures,
        }
    }
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum AssistantContent {
    Text {
        text: String,
        signature: Option<String>,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    ToolCall(AgentToolCall),
}

impl From<agent::AssistantContent> for AssistantContent {
    fn from(v: agent::AssistantContent) -> Self {
        match v {
            agent::AssistantContent::Text { text, signature } => Self::Text { text, signature },
            agent::AssistantContent::Thinking {
                thinking,
                signature,
            } => Self::Thinking {
                thinking,
                signature,
            },
            agent::AssistantContent::ToolCall(call) => Self::ToolCall(call.into()),
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    pub stop_reason: StopReason,
    pub provider_stop_reason: Option<String>,
    pub error_message: Option<String>,
    pub usage: AgentUsage,
    pub model: ModelIden,
    pub response_id: Option<String>,
    pub timestamp: i64,
}

impl From<agent::AssistantMessage> for AssistantMessage {
    fn from(v: agent::AssistantMessage) -> Self {
        Self {
            content: v.content.into_iter().map(Into::into).collect(),
            stop_reason: v.stop_reason.into(),
            provider_stop_reason: v.provider_stop_reason,
            error_message: v.error_message,
            usage: v.usage.into(),
            model: v.model.into(),
            response_id: v.response_id,
            timestamp: v.timestamp,
        }
    }
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum AgentMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    Custom(CustomMessage),
}

impl From<agent::AgentMessage> for AgentMessage {
    fn from(v: agent::AgentMessage) -> Self {
        match v {
            agent::AgentMessage::User(m) => Self::User(m.into()),
            agent::AgentMessage::Assistant(m) => Self::Assistant(m.into()),
            agent::AgentMessage::ToolResult(m) => Self::ToolResult(m.into()),
            agent::AgentMessage::Custom(m) => Self::Custom(m.into()),
        }
    }
}

// endregion: --- Message tree

// region:    --- Events (outbound)

/// Result payload of one tool execution.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AgentToolResult {
    pub content: Vec<ToolResultContent>,
    /// JSON text (core `details: Value`).
    pub details_json: String,
    pub usage: Option<AgentUsage>,
    pub added_tool_names: Vec<String>,
    pub terminate: bool,
}

impl From<agent::AgentToolResult> for AgentToolResult {
    fn from(v: agent::AgentToolResult) -> Self {
        Self {
            content: v.content.into_iter().map(Into::into).collect(),
            details_json: serde_json::to_string(&v.details).unwrap_or_default(),
            usage: v.usage.map(Into::into),
            added_tool_names: v.added_tool_names,
            terminate: v.terminate,
        }
    }
}

/// Provider-level streaming event; each variant carries the full accumulated
/// `partial` message (self-synchronizing — no delta stitching in the host).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum AssistantMessageEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: u32,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: u32,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: u32,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        content_index: u32,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: u32,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: u32,
        thinking: String,
        partial: AssistantMessage,
    },
    ToolCallStart {
        content_index: u32,
        partial: AssistantMessage,
    },
    ToolCallDelta {
        content_index: u32,
        delta: String,
        partial: AssistantMessage,
    },
    ToolCallEnd {
        content_index: u32,
        tool_call: AgentToolCall,
        partial: AssistantMessage,
    },
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
}

impl From<agent::AssistantMessageEvent> for AssistantMessageEvent {
    fn from(v: agent::AssistantMessageEvent) -> Self {
        use agent::AssistantMessageEvent as E;
        match v {
            E::Start { partial } => Self::Start {
                partial: partial.into(),
            },
            E::TextStart {
                content_index,
                partial,
            } => Self::TextStart {
                content_index,
                partial: partial.into(),
            },
            E::TextDelta {
                content_index,
                delta,
                partial,
            } => Self::TextDelta {
                content_index,
                delta,
                partial: partial.into(),
            },
            E::TextEnd {
                content_index,
                content,
                partial,
            } => Self::TextEnd {
                content_index,
                content,
                partial: partial.into(),
            },
            E::ThinkingStart {
                content_index,
                partial,
            } => Self::ThinkingStart {
                content_index,
                partial: partial.into(),
            },
            E::ThinkingDelta {
                content_index,
                delta,
                partial,
            } => Self::ThinkingDelta {
                content_index,
                delta,
                partial: partial.into(),
            },
            E::ThinkingEnd {
                content_index,
                thinking,
                partial,
            } => Self::ThinkingEnd {
                content_index,
                thinking,
                partial: partial.into(),
            },
            E::ToolCallStart {
                content_index,
                partial,
            } => Self::ToolCallStart {
                content_index,
                partial: partial.into(),
            },
            E::ToolCallDelta {
                content_index,
                delta,
                partial,
            } => Self::ToolCallDelta {
                content_index,
                delta,
                partial: partial.into(),
            },
            E::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            } => Self::ToolCallEnd {
                content_index,
                tool_call: tool_call.into(),
                partial: partial.into(),
            },
            E::Done { reason, message } => Self::Done {
                reason: reason.into(),
                message: message.into(),
            },
            E::Error { reason, error } => Self::Error {
                reason: reason.into(),
                error: error.into(),
            },
        }
    }
}

/// One agent-loop event. Awaited sequentially per event by the sink
/// (see `AgentEventSink`): the loop does not advance until `emit` resolves.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AgentMessage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args_json: String,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args_json: String,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
}

impl From<agent::AgentEvent> for AgentEvent {
    fn from(v: agent::AgentEvent) -> Self {
        use agent::AgentEvent as E;
        match v {
            E::AgentStart => Self::AgentStart,
            E::AgentEnd { messages } => Self::AgentEnd {
                messages: messages.into_iter().map(Into::into).collect(),
            },
            E::TurnStart => Self::TurnStart,
            E::TurnEnd {
                message,
                tool_results,
            } => Self::TurnEnd {
                message: message.into(),
                tool_results: tool_results.into_iter().map(Into::into).collect(),
            },
            E::MessageStart { message } => Self::MessageStart {
                message: message.into(),
            },
            E::MessageUpdate {
                message,
                assistant_message_event,
            } => Self::MessageUpdate {
                message: message.into(),
                assistant_message_event: assistant_message_event.into(),
            },
            E::MessageEnd { message } => Self::MessageEnd {
                message: message.into(),
            },
            E::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => Self::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args_json: serde_json::to_string(&args).unwrap_or_default(),
            },
            E::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => Self::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args_json: serde_json::to_string(&args).unwrap_or_default(),
                partial_result: partial_result.into(),
            },
            E::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => Self::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result: result.into(),
                is_error,
            },
        }
    }
}

// endregion: --- Events

// region:    --- State snapshot (outbound)

/// A render-friendly view of `Agent::state()` for hosts following the
/// "events as signals, render from state" pattern (docs/embedding.md §8).
/// Deliberately omits non-representable fields (tool objects, pending-call
/// sets) and the model spec.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AgentSnapshot {
    pub system_prompt: String,
    pub thinking_level: ThinkingLevel,
    pub messages: Vec<AgentMessage>,
    pub streaming_message: Option<AgentMessage>,
    pub is_streaming: bool,
}

impl From<agent::AgentState> for AgentSnapshot {
    fn from(v: agent::AgentState) -> Self {
        Self {
            system_prompt: v.system_prompt,
            thinking_level: v.thinking_level.into(),
            messages: v.messages.into_iter().map(Into::into).collect(),
            streaming_message: v.streaming_message.map(Into::into),
            is_streaming: v.is_streaming,
        }
    }
}

// endregion: --- State snapshot

// region:    --- Inbound conversions for the B1 message tree
//
// B2 made the message tree bidirectional (typed initial messages in
// AgentSetup, host-built tool results/hook outcomes). Conversions are
// field-wise; `#[non_exhaustive]` core structs build via Default + assign;
// `*_json` fields parse with Null-on-error here and are validated by the
// calling adapter before conversion (see lib.rs), so a Null cannot silently
// reach a provider.

impl From<UserContent> for agent::UserContent {
    fn from(v: UserContent) -> Self {
        match v {
            UserContent::Text { text } => Self::Text { text },
            UserContent::Image {
                data,
                mime_type,
                name,
            } => Self::Image {
                data,
                mime_type,
                name,
            },
        }
    }
}

impl From<UserMessage> for agent::UserMessage {
    fn from(v: UserMessage) -> Self {
        Self {
            content: v.content.into_iter().map(Into::into).collect(),
            timestamp: v.timestamp,
        }
    }
}

impl From<ToolResultContent> for agent::ToolResultContent {
    fn from(v: ToolResultContent) -> Self {
        match v {
            ToolResultContent::Text { text } => Self::Text { text },
            ToolResultContent::Image {
                data,
                mime_type,
                name,
            } => Self::Image {
                data,
                mime_type,
                name,
            },
        }
    }
}

impl From<ToolResultMessage> for agent::ToolResultMessage {
    fn from(v: ToolResultMessage) -> Self {
        Self {
            tool_call_id: v.tool_call_id,
            tool_name: v.tool_name,
            content: v.content.into_iter().map(Into::into).collect(),
            details: serde_json::from_str(&v.details_json).unwrap_or(serde_json::Value::Null),
            usage: v.usage.map(Into::into),
            added_tool_names: v.added_tool_names,
            is_error: v.is_error,
            timestamp: v.timestamp,
        }
    }
}

impl From<CustomMessage> for agent::CustomMessage {
    fn from(v: CustomMessage) -> Self {
        Self {
            role: v.role,
            data: serde_json::from_str(&v.data_json).unwrap_or(serde_json::Value::Null),
            timestamp: v.timestamp,
        }
    }
}

impl From<AgentToolCall> for agent::AgentToolCall {
    fn from(v: AgentToolCall) -> Self {
        Self {
            id: v.id,
            name: v.name,
            arguments: serde_json::from_str(&v.arguments_json).unwrap_or(serde_json::Value::Null),
            namespace: v.namespace,
            thought_signatures: v.thought_signatures,
        }
    }
}

impl From<AssistantContent> for agent::AssistantContent {
    fn from(v: AssistantContent) -> Self {
        match v {
            AssistantContent::Text { text, signature } => Self::Text { text, signature },
            AssistantContent::Thinking {
                thinking,
                signature,
            } => Self::Thinking {
                thinking,
                signature,
            },
            AssistantContent::ToolCall(call) => Self::ToolCall(call.into()),
        }
    }
}

impl From<ModelIden> for genai::ModelIden {
    fn from(v: ModelIden) -> Self {
        // AdapterKind round-trips through its serde (variant-name) form.
        let adapter_kind: genai::adapter::AdapterKind =
            serde_json::from_str(&format!("\"{}\"", v.adapter_kind))
                .unwrap_or_else(|_| panic!("unparseable adapter kind: {}", v.adapter_kind));
        genai::ModelIden::new(adapter_kind, genai::ModelName::from(v.model_name))
    }
}

impl From<AgentCost> for agent::AgentCost {
    fn from(v: AgentCost) -> Self {
        // #[non_exhaustive] core type: Default + assignment.
        let mut cost = Self::default();
        cost.input = v.input;
        cost.output = v.output;
        cost.cache_read = v.cache_read;
        cost.cache_write = v.cache_write;
        cost.total = v.total;
        cost
    }
}

impl From<AgentUsage> for agent::AgentUsage {
    fn from(v: AgentUsage) -> Self {
        // #[non_exhaustive] core type: Default + assignment.
        let mut usage = Self::default();
        usage.input_tokens = v.input_tokens;
        usage.output_tokens = v.output_tokens;
        usage.cache_read_tokens = v.cache_read_tokens;
        usage.cache_write_tokens = v.cache_write_tokens;
        usage.cache_write_1h_tokens = v.cache_write_1h_tokens;
        usage.cost = v.cost.map(Into::into);
        usage
    }
}

impl From<StopReason> for agent::StopReason {
    fn from(v: StopReason) -> Self {
        match v {
            StopReason::Pending => Self::Pending,
            StopReason::Stop => Self::Stop,
            StopReason::Length => Self::Length,
            StopReason::ToolUse => Self::ToolUse,
            StopReason::Error => Self::Error,
            StopReason::Aborted => Self::Aborted,
        }
    }
}

impl From<AssistantMessage> for agent::AssistantMessage {
    fn from(v: AssistantMessage) -> Self {
        Self {
            content: v.content.into_iter().map(Into::into).collect(),
            stop_reason: v.stop_reason.into(),
            provider_stop_reason: v.provider_stop_reason,
            error_message: v.error_message,
            usage: v.usage.into(),
            model: v.model.into(),
            response_id: v.response_id,
            timestamp: v.timestamp,
        }
    }
}

impl From<AgentMessage> for agent::AgentMessage {
    fn from(v: AgentMessage) -> Self {
        match v {
            AgentMessage::User(m) => Self::User(m.into()),
            AgentMessage::Assistant(m) => Self::Assistant(m.into()),
            AgentMessage::ToolResult(m) => Self::ToolResult(m.into()),
            AgentMessage::Custom(m) => Self::Custom(m.into()),
        }
    }
}

impl From<AgentToolResult> for agent::AgentToolResult {
    fn from(v: AgentToolResult) -> Self {
        Self {
            content: v.content.into_iter().map(Into::into).collect(),
            details: serde_json::from_str(&v.details_json).unwrap_or(serde_json::Value::Null),
            usage: v.usage.map(Into::into),
            added_tool_names: v.added_tool_names,
            terminate: v.terminate,
        }
    }
}

// endregion: --- Inbound conversions for the B1 message tree

// region:    --- ChatOptions tree (inbound; genai fork types)

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum CacheControl {
    Ephemeral,
    Memory,
    Ephemeral5m,
    Ephemeral1h,
}

impl From<CacheControl> for genai::chat::CacheControl {
    fn from(v: CacheControl) -> Self {
        match v {
            CacheControl::Ephemeral => Self::Ephemeral,
            CacheControl::Memory => Self::Memory,
            CacheControl::Ephemeral5m => Self::Ephemeral5m,
            CacheControl::Ephemeral1h => Self::Ephemeral1h,
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ReasoningEffort {
    Zero,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Budget(u32),
}

impl From<ReasoningEffort> for genai::chat::ReasoningEffort {
    fn from(v: ReasoningEffort) -> Self {
        match v {
            ReasoningEffort::Zero => Self::Zero,
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::XHigh => Self::XHigh,
            ReasoningEffort::Max => Self::Max,
            ReasoningEffort::Budget(tokens) => Self::Budget(tokens),
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum Verbosity {
    Low,
    Medium,
    High,
}

impl From<Verbosity> for genai::chat::Verbosity {
    fn from(v: Verbosity) -> Self {
        match v {
            Verbosity::Low => Self::Low,
            Verbosity::Medium => Self::Medium,
            Verbosity::High => Self::High,
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ServiceTier {
    Auto,
    Default,
    Flex,
}

impl From<ServiceTier> for genai::chat::ServiceTier {
    fn from(v: ServiceTier) -> Self {
        match v {
            ServiceTier::Auto => Self::Auto,
            ServiceTier::Default => Self::Default,
            ServiceTier::Flex => Self::Flex,
        }
    }
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool { name: String },
}

impl From<ToolChoice> for genai::chat::ToolChoice {
    fn from(v: ToolChoice) -> Self {
        match v {
            ToolChoice::Auto => Self::Auto,
            ToolChoice::None => Self::None,
            ToolChoice::Required => Self::Required,
            ToolChoice::Tool { name } => Self::Tool { name },
        }
    }
}

/// Structured-output JSON specification.
#[derive(Debug, Clone, uniffi::Record)]
pub struct JsonSpec {
    pub name: String,
    pub description: Option<String>,
    /// JSON text (core `schema: Value`).
    pub schema_json: String,
}

impl From<JsonSpec> for genai::chat::JsonSpec {
    fn from(v: JsonSpec) -> Self {
        Self {
            name: v.name,
            description: v.description,
            schema: serde_json::from_str(&v.schema_json).unwrap_or(serde_json::Value::Null),
        }
    }
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ChatResponseFormat {
    JsonMode,
    JsonSpec(JsonSpec),
}

impl From<ChatResponseFormat> for genai::chat::ChatResponseFormat {
    fn from(v: ChatResponseFormat) -> Self {
        match v {
            ChatResponseFormat::JsonMode => Self::JsonMode,
            ChatResponseFormat::JsonSpec(spec) => Self::JsonSpec(spec.into()),
        }
    }
}

/// Base provider chat options. All fields optional except `stop_sequences`
/// (empty = none) — mirrors `genai::chat::ChatOptions`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChatOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f64>,
    pub stop_sequences: Vec<String>,
    pub capture_usage: Option<bool>,
    pub capture_content: Option<bool>,
    pub capture_reasoning_content: Option<bool>,
    pub capture_tool_calls: Option<bool>,
    pub capture_raw_body: Option<bool>,
    pub response_format: Option<ChatResponseFormat>,
    pub tool_choice: Option<ToolChoice>,
    pub normalize_reasoning_content: Option<bool>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub verbosity: Option<Verbosity>,
    pub seed: Option<u64>,
    pub service_tier: Option<ServiceTier>,
    pub extra_headers: Option<std::collections::HashMap<String, String>>,
    pub cache_control: Option<CacheControl>,
    pub prompt_cache_key: Option<String>,
    /// JSON text merged into the request body (core `extra_body: Value`).
    pub extra_body_json: Option<String>,
}

impl From<ChatOptions> for genai::chat::ChatOptions {
    fn from(v: ChatOptions) -> Self {
        Self {
            temperature: v.temperature,
            max_tokens: v.max_tokens,
            top_p: v.top_p,
            stop_sequences: v.stop_sequences,
            capture_usage: v.capture_usage,
            capture_content: v.capture_content,
            capture_reasoning_content: v.capture_reasoning_content,
            capture_tool_calls: v.capture_tool_calls,
            capture_raw_body: v.capture_raw_body,
            response_format: v.response_format.map(Into::into),
            tool_choice: v.tool_choice.map(Into::into),
            normalize_reasoning_content: v.normalize_reasoning_content,
            reasoning_effort: v.reasoning_effort.map(Into::into),
            verbosity: v.verbosity.map(Into::into),
            seed: v.seed,
            service_tier: v.service_tier.map(Into::into),
            extra_headers: v
                .extra_headers
                .map(|h| genai::Headers::from(h.into_iter().collect::<Vec<_>>())),
            cache_control: v.cache_control.map(Into::into),
            prompt_cache_key: v.prompt_cache_key,
            extra_body: v
                .extra_body_json
                .map(|json| serde_json::from_str(&json).unwrap_or(serde_json::Value::Null)),
        }
    }
}

// endregion: --- ChatOptions tree

// region:    --- Tools & hooks (B2)

/// Provider-facing tool declaration, implemented by host tools.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ToolSpec {
    pub name: String,
    pub label: String,
    pub description: String,
    /// JSON text (core `schema: Value`) — the tool's parameter JSON schema.
    pub schema_json: String,
    pub strict: Option<bool>,
}

impl From<ToolSpec> for agent::ToolSpec {
    fn from(v: ToolSpec) -> Self {
        Self {
            name: v.name,
            label: v.label,
            description: v.description,
            schema: serde_json::from_str(&v.schema_json).unwrap_or(serde_json::Value::Null),
            strict: v.strict,
        }
    }
}

impl From<agent::ToolSpec> for ToolSpec {
    fn from(v: agent::ToolSpec) -> Self {
        Self {
            name: v.name,
            label: v.label,
            description: v.description,
            schema_json: serde_json::to_string(&v.schema).unwrap_or_default(),
            strict: v.strict,
        }
    }
}

/// Execution context handed to a host tool.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ToolCallContext {
    pub tool_call_id: String,
    pub tool_name: String,
    /// JSON text (core `args: Value`).
    pub args_json: String,
}

impl From<agent::ToolCallContext> for ToolCallContext {
    fn from(v: agent::ToolCallContext) -> Self {
        Self {
            tool_call_id: v.tool_call_id,
            tool_name: v.tool_name,
            args_json: serde_json::to_string(&v.args).unwrap_or_default(),
        }
    }
}

/// Data projection of the loop context handed to hooks. `tools` are
/// projected to their specs (the trait objects can't cross); rebuilding a
/// core `AgentContext` from this reuses the running config's tools — hosts
/// can change prompt/messages, not the tool set.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AgentContextData {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<ToolSpec>,
}

impl AgentContextData {
    /// Outbound projection; `tools` comes from the core context's tool objects.
    pub(crate) fn project(context: &agent::AgentContext) -> Self {
        Self {
            system_prompt: context.system_prompt.clone(),
            messages: context.messages.iter().cloned().map(Into::into).collect(),
            tools: context
                .tools
                .iter()
                .map(|tool| tool.spec().into())
                .collect(),
        }
    }

    /// Inbound rebuild; `tools` must come from the original (running) context.
    pub(crate) fn into_core(self, tools: Vec<Arc<dyn agent::AgentTool>>) -> agent::AgentContext {
        agent::AgentContext {
            system_prompt: self.system_prompt,
            messages: self.messages.into_iter().map(Into::into).collect(),
            tools,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    /// JSON text (core `args: Value`).
    pub args_json: String,
}

impl From<agent::BeforeToolCallContext> for BeforeToolCallContext {
    fn from(v: agent::BeforeToolCallContext) -> Self {
        Self {
            assistant_message: v.assistant_message.into(),
            tool_call: v.tool_call.into(),
            args_json: serde_json::to_string(&v.args).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    /// End the loop after blocking.
    pub terminate: bool,
}

impl From<BeforeToolCallResult> for agent::BeforeToolCallResult {
    fn from(v: BeforeToolCallResult) -> Self {
        Self {
            block: v.block,
            reason: v.reason,
            terminate: v.terminate,
        }
    }
}

/// Host decision for `BeforeToolCallHook`: optional argument rewrite +
/// optional block/allow. `None` fields leave the call untouched.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BeforeToolCallOutcome {
    /// JSON text; parsed `Value` when `Some`.
    pub args_json: Option<String>,
    pub decision: Option<BeforeToolCallResult>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    /// JSON text (core `args: Value`).
    pub args_json: String,
    pub result: AgentToolResult,
    pub is_error: bool,
}

impl From<agent::AfterToolCallContext> for AfterToolCallContext {
    fn from(v: agent::AfterToolCallContext) -> Self {
        Self {
            assistant_message: v.assistant_message.into(),
            tool_call: v.tool_call.into(),
            args_json: serde_json::to_string(&v.args).unwrap_or_default(),
            result: v.result.into(),
            is_error: v.is_error,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<ToolResultContent>>,
    /// JSON text; parsed `Value` when `Some`.
    pub details_json: Option<String>,
    pub is_error: Option<bool>,
    pub usage: Option<AgentUsage>,
    pub terminate: Option<bool>,
}

impl From<AfterToolCallResult> for agent::AfterToolCallResult {
    fn from(v: AfterToolCallResult) -> Self {
        Self {
            content: v
                .content
                .map(|parts| parts.into_iter().map(Into::into).collect()),
            details: v
                .details_json
                .map(|json| serde_json::from_str(&json).unwrap_or(serde_json::Value::Null)),
            is_error: v.is_error,
            usage: v.usage.map(Into::into),
            terminate: v.terminate,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct TurnContext {
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: AgentContextData,
}

impl From<agent::ShouldStopAfterTurnContext> for TurnContext {
    /// Projection of core `ShouldStopAfterTurnContext` (aka
    /// `PrepareNextTurnContext`); tool objects become their specs.
    fn from(v: agent::ShouldStopAfterTurnContext) -> Self {
        Self {
            message: v.message.into(),
            tool_results: v.tool_results.into_iter().map(Into::into).collect(),
            context: AgentContextData::project(&v.context),
        }
    }
}

/// Model override for the next turn. `ModelSpec::Target` never crosses (it
/// carries a resolved service target, including auth).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ModelSpec {
    Name(String),
    Iden(ModelIden),
}

impl From<ModelSpec> for genai::ModelSpec {
    fn from(v: ModelSpec) -> Self {
        match v {
            ModelSpec::Name(name) => Self::Name(genai::ModelName::from(name)),
            ModelSpec::Iden(iden) => Self::Iden(iden.into()),
        }
    }
}

/// Replacements applied before the next turn (host returns from
/// `PrepareNextTurnHook`). A `None` field keeps the current value.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContextData>,
    pub model: Option<ModelSpec>,
    pub thinking_level: Option<ThinkingLevel>,
}

// endregion: --- Tools & hooks
