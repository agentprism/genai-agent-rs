//! Widened agent transcript messages and conversion to provider chat messages.
//!
//! The agent retains user, assistant, tool-result, and application-defined custom messages. The
//! transcript is narrowed to `genai` messages only at the provider boundary through
//! [`ConvertToLlm`], allowing applications to define how custom messages participate in prompts.

use crate::assistant::{AgentUsage, AssistantContent, AssistantMessage, timestamp_ms};
use futures::future::BoxFuture;
use genai::chat::{Binary, ChatMessage, ContentPart, MessageContent, ToolResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// Content accepted in a user message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContent {
    /// A user-authored text block.
    Text {
        /// Text sent to the provider.
        text: String,
    },
    /// A base64-encoded image attachment.
    Image {
        /// Base64-encoded image bytes.
        data: String,
        /// Media type for the image, such as `image/png`.
        mime_type: String,
        /// Optional filename or provider-facing attachment name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl UserContent {
    /// Construct a user text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Construct an unnamed base64-encoded image block.
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
            name: None,
        }
    }
}

/// A user turn in the widened agent transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    /// Ordered content blocks in the user turn.
    pub content: Vec<UserContent>,
    /// Message creation time in milliseconds since the Unix epoch.
    pub timestamp: i64,
}

impl UserMessage {
    /// Construct a user turn with the current timestamp.
    pub fn new(content: Vec<UserContent>) -> Self {
        Self {
            content,
            timestamp: timestamp_ms(),
        }
    }

    /// Construct a user turn containing one text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(vec![UserContent::text(text)])
    }
}

/// Text or image returned by a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    /// Text produced by a tool.
    Text {
        /// Text retained in the transcript and returned to the provider.
        text: String,
    },
    /// A base64-encoded image produced by a tool.
    Image {
        /// Base64-encoded image bytes.
        data: String,
        /// Media type for the image, such as `image/png`.
        mime_type: String,
        /// Optional filename or attachment name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl ToolResultContent {
    /// Construct a tool-result text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Construct an unnamed base64-encoded image block.
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
            name: None,
        }
    }
}

/// A tool response stored in the agent transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// Identifier of the assistant tool call this result answers.
    pub tool_call_id: String,
    /// Name of the tool that produced the result.
    pub tool_name: String,
    /// Ordered provider-facing result content.
    pub content: Vec<ToolResultContent>,
    /// Application-defined structured detail retained outside the default provider prompt.
    pub details: Value,
    /// Optional resource usage attributable to the tool execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsage>,
    /// Names of tools made available as a consequence of this result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_tool_names: Vec<String>,
    /// Whether execution produced an error result rather than a successful result.
    pub is_error: bool,
    /// Message creation time in milliseconds since the Unix epoch.
    pub timestamp: i64,
}

impl ToolResultMessage {
    /// Construct a successful result with null details and the current timestamp.
    pub fn new(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: Vec<ToolResultContent>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content,
            details: Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: timestamp_ms(),
        }
    }

    /// Construct a successful result containing one text block.
    pub fn text(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::new(tool_call_id, tool_name, vec![ToolResultContent::text(text)])
    }
}

/// Open, application-defined transcript message.
///
/// The default provider converter deliberately drops custom messages. Supply a custom
/// [`ConvertToLlm`] when a role should be translated into provider input; the original message
/// remains available to context transforms and other agent-side logic regardless of conversion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomMessage {
    /// Application-defined role returned by [`AgentMessage::role`].
    pub role: String,
    /// Arbitrary application-defined payload.
    pub data: Value,
    /// Optional creation time in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
}

impl CustomMessage {
    /// Construct a custom message with the current timestamp.
    pub fn new(role: impl Into<String>, data: Value) -> Self {
        Self {
            role: role.into(),
            data,
            timestamp: Some(timestamp_ms()),
        }
    }
}

/// Transcript message type used throughout the agent. Conversion to `genai` messages happens
/// only at the provider boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", content = "message", rename_all = "snake_case")]
pub enum AgentMessage {
    /// Input supplied by a user.
    User(UserMessage),
    /// A partial or completed model response.
    Assistant(AssistantMessage),
    /// The response correlated with an assistant tool call.
    ToolResult(ToolResultMessage),
    /// An application-defined message interpreted by a custom converter or hook.
    Custom(CustomMessage),
}

impl AgentMessage {
    /// Construct a user message containing one text block.
    pub fn user(text: impl Into<String>) -> Self {
        Self::User(UserMessage::text(text))
    }

    /// Wrap an assistant message as a transcript message.
    pub fn assistant(message: AssistantMessage) -> Self {
        Self::Assistant(message)
    }

    /// Return the normalized transcript role, or the application-defined custom role.
    pub fn role(&self) -> &str {
        match self {
            Self::User(_) => "user",
            Self::Assistant(_) => "assistant",
            Self::ToolResult(_) => "tool_result",
            Self::Custom(message) => &message.role,
        }
    }

    /// Return whether this is an [`AgentMessage::Assistant`] variant.
    pub fn is_assistant(&self) -> bool {
        matches!(self, Self::Assistant(_))
    }

    /// Return the message timestamp in Unix-epoch milliseconds when one is present.
    ///
    /// Standard message variants always contain a timestamp; a custom message may omit it.
    pub fn timestamp(&self) -> Option<i64> {
        match self {
            Self::User(message) => Some(message.timestamp),
            Self::Assistant(message) => Some(message.timestamp),
            Self::ToolResult(message) => Some(message.timestamp),
            Self::Custom(message) => message.timestamp,
        }
    }
}

impl From<UserMessage> for AgentMessage {
    fn from(value: UserMessage) -> Self {
        Self::User(value)
    }
}

impl From<AssistantMessage> for AgentMessage {
    fn from(value: AssistantMessage) -> Self {
        Self::Assistant(value)
    }
}

impl From<ToolResultMessage> for AgentMessage {
    fn from(value: ToolResultMessage) -> Self {
        Self::ToolResult(value)
    }
}

/// Async one-way bridge from the widened transcript to provider chat messages.
///
/// The converter receives an owned transcript snapshot and may filter, reorder, expand, or map
/// messages before an invocation. This is the extension point for translating [`CustomMessage`];
/// no reverse conversion is performed after the provider responds.
pub type ConvertToLlm =
    Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Vec<ChatMessage>> + Send + Sync>;

/// Construct the default provider-message converter.
///
/// User text and images become user content parts. Assistant text, reasoning, signatures, and tool
/// calls are preserved. Tool-result text blocks are joined with newlines, while each image becomes
/// the literal placeholder `[image omitted]`; tool metadata is not sent. [`CustomMessage`] values
/// are intentionally filtered out.
pub fn default_convert_to_llm() -> ConvertToLlm {
    Arc::new(|messages| Box::pin(async move { convert_messages_to_llm(&messages) }))
}

/// Synchronously apply the standard conversion used by [`default_convert_to_llm`].
///
/// This is useful when composing a custom asynchronous converter: applications can transform or
/// remove their custom variants and delegate the remaining standard messages here.
pub fn convert_messages_to_llm(messages: &[AgentMessage]) -> Vec<ChatMessage> {
    messages.iter().filter_map(convert_message_to_llm).collect()
}

fn convert_message_to_llm(message: &AgentMessage) -> Option<ChatMessage> {
    match message {
        AgentMessage::User(user) => {
            let parts = user
                .content
                .iter()
                .map(|part| match part {
                    UserContent::Text { text } => ContentPart::Text(text.clone()),
                    UserContent::Image {
                        data,
                        mime_type,
                        name,
                    } => ContentPart::Binary(Binary::from_base64(
                        mime_type.clone(),
                        data.clone(),
                        name.clone(),
                    )),
                })
                .collect::<Vec<_>>();
            Some(ChatMessage::user(MessageContent::from_parts(parts)))
        }
        AgentMessage::Assistant(assistant) => {
            let mut parts = Vec::new();
            for part in &assistant.content {
                match part {
                    AssistantContent::Text { text, signature } => {
                        parts.push(ContentPart::Text(text.clone()));
                        if let Some(signature) = signature {
                            parts.push(ContentPart::ThoughtSignature(signature.clone()));
                        }
                    }
                    AssistantContent::Thinking {
                        thinking,
                        signature,
                    } => {
                        parts.push(ContentPart::ReasoningContent(thinking.clone()));
                        if let Some(signature) = signature {
                            parts.push(ContentPart::ThoughtSignature(signature.clone()));
                        }
                    }
                    AssistantContent::ToolCall(call) => {
                        for signature in &call.thought_signatures {
                            parts.push(ContentPart::ThoughtSignature(signature.clone()));
                        }
                        parts.push(ContentPart::ToolCall(call.clone().into()));
                    }
                }
            }
            Some(ChatMessage::assistant(MessageContent::from_parts(parts)))
        }
        AgentMessage::ToolResult(result) => {
            let content = result
                .content
                .iter()
                .map(|part| match part {
                    ToolResultContent::Text { text } => text.clone(),
                    ToolResultContent::Image { .. } => "[image omitted]".to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(ChatMessage::tool(ToolResponse {
                call_id: result.tool_call_id.clone(),
                fn_name: Some(result.tool_name.clone()),
                content,
            }))
        }
        AgentMessage::Custom(_) => None,
    }
}
