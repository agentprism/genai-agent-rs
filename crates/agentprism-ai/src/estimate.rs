//! Pi-equivalent context-token estimation used by simple option planning.
//!
//! Pinned Pi deliberately uses a lightweight character estimator rather than
//! a provider tokenizer. Keeping the same estimator is required because its
//! result affects the provider-visible maximum-output-token field.

use crate::{
    AssistantFinishReason, ContentBlock, Context, LoweringError, Message, ToolResultContent,
    ToolSpec, Usage,
};
use serde::Serialize;

/// Number of JavaScript UTF-16 code units estimated per token by pinned Pi.
pub const ESTIMATED_CHARS_PER_TOKEN: u64 = 4;

/// Character charge assigned to one image by pinned Pi.
pub const ESTIMATED_IMAGE_CHARS: u64 = 4_800;

/// Detailed context estimate matching `packages/ai/src/utils/estimate.ts`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextUsageEstimate {
    /// Estimated total context tokens.
    pub tokens: u64,
    /// Tokens reported by the most recent applicable assistant response.
    pub usage_tokens: u64,
    /// Estimated tokens after that response.
    pub trailing_tokens: u64,
    /// Index of the assistant response supplying `usage_tokens`.
    pub last_usage_index: Option<usize>,
}

/// Object-safe token-estimation seam used by [`crate::plan_common`].
pub trait TokenEstimator: Send + Sync {
    /// Estimates canonical context tokens.
    fn estimate(&self, context: &Context) -> Result<u64, LoweringError>;
}

/// The lightweight, deterministic estimator used by pinned Pi.
#[derive(Clone, Copy, Debug, Default)]
pub struct PiTokenEstimator;

impl TokenEstimator for PiTokenEstimator {
    fn estimate(&self, context: &Context) -> Result<u64, LoweringError> {
        estimate_context_tokens(context).map(|estimate| estimate.tokens)
    }
}

/// Calculates the usage value Pi treats as the already-accounted prefix.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    u64::try_from(usage.total_tokens()).unwrap_or(u64::MAX)
}

/// Estimates tokens for UTF-8 text using JavaScript string-length semantics.
pub fn estimate_text_tokens(text: &str) -> u64 {
    chars_to_tokens(utf16_len(text))
}

/// Estimates one canonical message without system-prompt or tool overhead.
pub fn estimate_message_tokens(message: &Message) -> Result<u64, LoweringError> {
    let chars = match message {
        Message::User(message) => estimate_content_chars(&message.content)?,
        Message::ToolResult(message) => estimate_tool_result_content_chars(&message.content),
        Message::Assistant(message) => estimate_content_chars(&message.content)?,
    };
    Ok(chars_to_tokens(chars))
}

/// Estimates a full canonical context with Pi's usage-aware prefix rules.
pub fn estimate_context_tokens(context: &Context) -> Result<ContextUsageEstimate, LoweringError> {
    let mut latest_prefix_timestamp = i64::MIN;
    let mut usage_info = None;

    for (index, message) in context.messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            let usage_tokens = calculate_context_tokens(&assistant.usage);
            let usage_applies_to_prefix =
                assistant.timestamp.unix_millis() >= latest_prefix_timestamp;
            if usage_applies_to_prefix
                && !matches!(
                    assistant.finish.reason,
                    AssistantFinishReason::Aborted | AssistantFinishReason::Error
                )
                && usage_tokens > 0
            {
                usage_info = Some((index, usage_tokens));
            }
        }
        latest_prefix_timestamp = latest_prefix_timestamp.max(message_timestamp(message));
    }

    if let Some((last_usage_index, usage_tokens)) = usage_info {
        let mut trailing_tokens = 0_u64;
        for message in &context.messages[last_usage_index + 1..] {
            trailing_tokens = trailing_tokens.saturating_add(estimate_message_tokens(message)?);
        }

        let added_names = context.messages[last_usage_index + 1..]
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(message) => Some(message.added_tool_names.as_slice()),
                Message::User(_) | Message::Assistant(_) => None,
            })
            .flatten()
            .collect::<std::collections::BTreeSet<_>>();
        let added_tool_tokens = estimate_tools_tokens(
            context
                .tools
                .iter()
                .filter(|tool| added_names.contains(&tool.name))
                .collect::<Vec<_>>()
                .as_slice(),
        )?;
        trailing_tokens = trailing_tokens.saturating_add(added_tool_tokens);

        return Ok(ContextUsageEstimate {
            tokens: usage_tokens.saturating_add(trailing_tokens),
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(last_usage_index),
        });
    }

    let mut message_tokens = 0_u64;
    for message in &context.messages {
        message_tokens = message_tokens.saturating_add(estimate_message_tokens(message)?);
    }
    let prefix_tokens = context
        .system_prompt
        .as_deref()
        .map_or(0, estimate_text_tokens)
        .saturating_add(estimate_tools_tokens(
            context.tools.iter().collect::<Vec<_>>().as_slice(),
        )?);
    let trailing_tokens = message_tokens.saturating_add(prefix_tokens);

    Ok(ContextUsageEstimate {
        tokens: trailing_tokens,
        usage_tokens: 0,
        trailing_tokens,
        last_usage_index: None,
    })
}

fn estimate_content_chars(content: &[ContentBlock]) -> Result<u64, LoweringError> {
    content.iter().try_fold(0_u64, |total, block| {
        let chars = match block {
            ContentBlock::Text { text, .. } | ContentBlock::Thinking { text, .. } => {
                utf16_len(text)
            }
            ContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
            ContentBlock::ToolCall { call, .. } => {
                utf16_len(&call.name).saturating_add(serialized_utf16_len(&call.arguments)?)
            }
        };
        Ok(total.saturating_add(chars))
    })
}

fn estimate_tool_result_content_chars(content: &[ToolResultContent]) -> u64 {
    content.iter().fold(0_u64, |total, block| {
        total.saturating_add(match block {
            ToolResultContent::Text { text, .. } => utf16_len(text),
            ToolResultContent::Image { .. } => ESTIMATED_IMAGE_CHARS,
        })
    })
}

fn estimate_tools_tokens(tools: &[&ToolSpec]) -> Result<u64, LoweringError> {
    if tools.is_empty() {
        return Ok(0);
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct EstimatedTool<'a> {
        name: &'a str,
        description: &'a str,
        parameters: &'a serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        constrained_sampling: Option<&'a crate::ConstrainedSampling>,
    }

    let estimated = tools
        .iter()
        .map(|tool| EstimatedTool {
            name: &tool.name,
            description: &tool.description,
            parameters: &tool.parameters,
            constrained_sampling: tool.constrained_sampling.as_ref(),
        })
        .collect::<Vec<_>>();
    Ok(chars_to_tokens(serialized_utf16_len(&estimated)?))
}

fn serialized_utf16_len(value: &impl Serialize) -> Result<u64, LoweringError> {
    crate::json_stringify_compatible(value)
        .map(|json| utf16_len(&json))
        .map_err(|error| LoweringError::InvalidConfiguration {
            message: format!("context token estimation could not serialize JSON: {error}"),
        })
}

fn utf16_len(text: &str) -> u64 {
    u64::try_from(text.encode_utf16().count()).unwrap_or(u64::MAX)
}

fn chars_to_tokens(chars: u64) -> u64 {
    chars.saturating_add(ESTIMATED_CHARS_PER_TOKEN - 1) / ESTIMATED_CHARS_PER_TOKEN
}

fn message_timestamp(message: &Message) -> i64 {
    match message {
        Message::User(message) => message.timestamp.unix_millis(),
        Message::Assistant(message) => message.timestamp.unix_millis(),
        Message::ToolResult(message) => message.timestamp.unix_millis(),
    }
}
