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
///
/// B1 scope: `chat_options` (provider options) is not yet exposed; it defaults.
/// The initial transcript crosses as `initial_messages_json` (a JSON array of
/// core `AgentMessage`, per its serde shape) until typed message construction
/// lands with the tool surface in B2.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AgentSetup {
	pub system_prompt: String,
	pub model: String,
	pub session_id: Option<String>,
	pub thinking_level: ThinkingLevel,
	pub thinking_budgets: Option<ThinkingBudgets>,
	pub max_retries: Option<u32>,
	pub max_retry_delay_ms: Option<u64>,
	pub tool_execution: ToolExecutionMode,
	pub transport: Transport,
	pub steering_mode: QueueMode,
	pub follow_up_mode: QueueMode,
	pub initial_messages_json: Option<String>,
}

impl AgentSetup {
	pub(crate) fn try_into_core(self) -> Result<agent::AgentSetup, AgentError> {
		let messages: Vec<agent::AgentMessage> = match &self.initial_messages_json {
			Some(json) => serde_json::from_str(json)
				.map_err(|err| AgentError::Other(format!("initial_messages_json: {err}")))?,
			None => Vec::new(),
		};
		Ok(agent::AgentSetup {
			system_prompt: self.system_prompt,
			model: self.model,
			session_id: self.session_id,
			messages,
			thinking_level: self.thinking_level.into(),
			thinking_budgets: self.thinking_budgets.map(Into::into),
			max_retries: self.max_retries,
			max_retry_delay_ms: self.max_retry_delay_ms,
			tool_execution: self.tool_execution.into(),
			transport: self.transport.into(),
			steering_mode: self.steering_mode.into(),
			follow_up_mode: self.follow_up_mode.into(),
			chat_options: Default::default(),
		})
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
	Text { text: String },
	Image { data: String, mime_type: String, name: Option<String> },
}

impl From<agent::UserContent> for UserContent {
	fn from(v: agent::UserContent) -> Self {
		match v {
			agent::UserContent::Text { text } => Self::Text { text },
			agent::UserContent::Image { data, mime_type, name } => Self::Image { data, mime_type, name },
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
	Text { text: String },
	Image { data: String, mime_type: String, name: Option<String> },
}

impl From<agent::ToolResultContent> for ToolResultContent {
	fn from(v: agent::ToolResultContent) -> Self {
		match v {
			agent::ToolResultContent::Text { text } => Self::Text { text },
			agent::ToolResultContent::Image { data, mime_type, name } => Self::Image { data, mime_type, name },
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
	pub thought_signatures: Vec<String>,
}

impl From<agent::AgentToolCall> for AgentToolCall {
	fn from(v: agent::AgentToolCall) -> Self {
		Self {
			id: v.id,
			name: v.name,
			arguments_json: serde_json::to_string(&v.arguments).unwrap_or_default(),
			thought_signatures: v.thought_signatures,
		}
	}
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum AssistantContent {
	Text { text: String, signature: Option<String> },
	Thinking { thinking: String, signature: Option<String> },
	ToolCall(AgentToolCall),
}

impl From<agent::AssistantContent> for AssistantContent {
	fn from(v: agent::AssistantContent) -> Self {
		match v {
			agent::AssistantContent::Text { text, signature } => Self::Text { text, signature },
			agent::AssistantContent::Thinking { thinking, signature } => Self::Thinking { thinking, signature },
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
	Start { partial: AssistantMessage },
	TextStart { content_index: u32, partial: AssistantMessage },
	TextDelta { content_index: u32, delta: String, partial: AssistantMessage },
	TextEnd { content_index: u32, content: String, partial: AssistantMessage },
	ThinkingStart { content_index: u32, partial: AssistantMessage },
	ThinkingDelta { content_index: u32, delta: String, partial: AssistantMessage },
	ThinkingEnd { content_index: u32, thinking: String, partial: AssistantMessage },
	ToolCallStart { content_index: u32, partial: AssistantMessage },
	ToolCallDelta { content_index: u32, delta: String, partial: AssistantMessage },
	ToolCallEnd { content_index: u32, tool_call: AgentToolCall, partial: AssistantMessage },
	Done { reason: StopReason, message: AssistantMessage },
	Error { reason: StopReason, error: AssistantMessage },
}

impl From<agent::AssistantMessageEvent> for AssistantMessageEvent {
	fn from(v: agent::AssistantMessageEvent) -> Self {
		use agent::AssistantMessageEvent as E;
		match v {
			E::Start { partial } => Self::Start { partial: partial.into() },
			E::TextStart { content_index, partial } => Self::TextStart {
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
			E::ThinkingStart { content_index, partial } => Self::ThinkingStart {
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
			E::ToolCallStart { content_index, partial } => Self::ToolCallStart {
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
	AgentEnd { messages: Vec<AgentMessage> },
	TurnStart,
	TurnEnd { message: AgentMessage, tool_results: Vec<ToolResultMessage> },
	MessageStart { message: AgentMessage },
	MessageUpdate { message: AgentMessage, assistant_message_event: AssistantMessageEvent },
	MessageEnd { message: AgentMessage },
	ToolExecutionStart { tool_call_id: String, tool_name: String, args_json: String },
	ToolExecutionUpdate { tool_call_id: String, tool_name: String, args_json: String, partial_result: AgentToolResult },
	ToolExecutionEnd { tool_call_id: String, tool_name: String, result: AgentToolResult, is_error: bool },
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
			E::TurnEnd { message, tool_results } => Self::TurnEnd {
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
