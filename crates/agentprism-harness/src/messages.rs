//! Pinned Pi harness resource-message construction and model projection.

use crate::compaction::{BASH_EXECUTION_ROLE, CUSTOM_ROLE, project_record_to_message};
use agentprism_ai::{ContentBlock, Message, Timestamp};
use agentprism_core::AgentRecord;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::fmt::Write as _;

/// Native persisted bash-execution payload schema.
pub const BASH_EXECUTION_MESSAGE_SCHEMA_VERSION: u32 = 1;

/// Native persisted custom-message payload schema.
pub const CUSTOM_HARNESS_MESSAGE_SCHEMA_VERSION: u32 = 1;

/// Persisted payload for Pi's `bashExecution` custom agent message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    /// Native payload schema.
    pub schema_version: u32,
    /// Executed command as displayed to the model.
    pub command: String,
    /// Captured bounded command output.
    pub output: String,
    /// Exit status when the process produced one.
    pub exit_code: Option<i32>,
    /// Whether cancellation terminated the command.
    pub cancelled: bool,
    /// Whether displayed output was truncated.
    pub truncated: bool,
    /// Durable location of complete output when truncation occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// Message creation time.
    pub timestamp: Timestamp,
    /// Excludes this record from provider context while retaining it durably.
    #[serde(default, skip_serializing_if = "is_false")]
    pub exclude_from_context: bool,
}

/// Persisted payload for Pi's generic `custom` harness message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomHarnessMessage {
    /// Native payload schema.
    pub schema_version: u32,
    /// Application-defined custom subtype.
    #[serde(rename = "customType")]
    pub custom_type: String,
    /// Text or canonical content blocks projected as a user message.
    pub content: serde_json::Value,
    /// Whether user interfaces should display the record.
    pub display: bool,
    /// Optional application detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    /// Message creation time.
    pub timestamp: Timestamp,
}

/// Formats a bash execution exactly as pinned Pi's `bashExecutionToText`.
pub fn format_bash_execution(message: &BashExecutionMessage) -> String {
    let mut text = format!("Ran `{}`\n", message.command);
    if message.output.is_empty() {
        text.push_str("(no output)");
    } else {
        write!(text, "```\n{}\n```", message.output)
            .expect("writing to an owned String cannot fail");
    }
    if message.cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(exit_code) = message.exit_code.filter(|code| *code != 0) {
        write!(text, "\n\nCommand exited with code {exit_code}")
            .expect("writing to an owned String cannot fail");
    }
    if message.truncated
        && let Some(path) = message.full_output_path.as_deref()
    {
        write!(text, "\n\n[Output truncated. Full output: {path}]")
            .expect("writing to an owned String cannot fail");
    }
    text
}

/// Creates a durable custom record carrying a bash execution payload.
pub fn bash_execution_record(
    message: &BashExecutionMessage,
) -> Result<AgentRecord, serde_json::Error> {
    custom_record(BASH_EXECUTION_ROLE, message)
}

/// Creates a durable custom record carrying Pi's generic custom-message payload.
pub fn custom_harness_record(
    message: &CustomHarnessMessage,
) -> Result<AgentRecord, serde_json::Error> {
    custom_record(CUSTOM_ROLE, message)
}

/// Projects known harness records and passes canonical messages through.
///
/// Unrecognized custom kinds remain UI-only and are omitted, matching pinned
/// Pi's switch default. `bashExecution.excludeFromContext` is also honored.
pub fn convert_harness_records_to_llm(records: &[AgentRecord]) -> Vec<Message> {
    records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match record {
            AgentRecord::Llm(message) => Some(message.clone()),
            AgentRecord::Custom { .. } => project_record_to_message(record, index),
        })
        .collect()
}

/// Returns concatenated text from a projected canonical message.
///
/// This small helper is useful to test and display resource formatting without
/// exposing provider wire conversion.
pub fn projected_message_text(message: &Message) -> String {
    match message {
        Message::User(message) => content_block_text(&message.content),
        Message::Assistant(message) => content_block_text(&message.content),
        Message::ToolResult(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                agentprism_ai::ToolResultContent::Text { text, .. } => Some(text.as_str()),
                agentprism_ai::ToolResultContent::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn content_block_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            ContentBlock::Image { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::ToolCall { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn custom_record(
    type_name: &str,
    value: &impl Serialize,
) -> Result<AgentRecord, serde_json::Error> {
    let json = serde_json::to_string(value)?;
    let payload = RawValue::from_string(json)?;
    Ok(AgentRecord::Custom {
        type_name: type_name.to_owned(),
        payload,
    })
}
