//! Context-token estimates ⇐ pi `src/utils/estimate.ts`.

use crate::types::{
    AssistantContent, Context, Message, StopReason, Tool, Usage, UserContent, UserContentBlock,
};
use crate::utils::ecma_json::stringify_object;
use std::collections::BTreeSet;

const CHARS_PER_TOKEN: usize = 4;
const ESTIMATED_IMAGE_CHARS: usize = 4_800;

#[derive(Debug, Clone, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: f64,
    pub usage_tokens: f64,
    pub trailing_tokens: f64,
    pub last_usage_index: Option<usize>,
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn chars_to_tokens(chars: usize) -> f64 {
    chars.div_ceil(CHARS_PER_TOKEN) as f64
}

pub fn calculate_context_tokens(usage: &Usage) -> f64 {
    if usage.total_tokens != 0.0 && !usage.total_tokens.is_nan() {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

pub fn estimate_text_tokens(text: &str) -> f64 {
    chars_to_tokens(utf16_len(text))
}

fn content_chars(content: &UserContent) -> usize {
    match content {
        UserContent::Text(text) => text.len(),
        UserContent::Blocks(blocks) => blocks.iter().map(block_chars).sum(),
    }
}

fn block_chars(block: &UserContentBlock) -> usize {
    match block {
        UserContentBlock::Text(text) => text.text.len(),
        UserContentBlock::Image(_) => ESTIMATED_IMAGE_CHARS,
    }
}

pub fn estimate_text_and_image_content_tokens(content: &UserContent) -> f64 {
    chars_to_tokens(content_chars(content))
}

pub fn estimate_message_tokens(message: &Message) -> f64 {
    match message {
        Message::User(message) => estimate_text_and_image_content_tokens(&message.content),
        Message::ToolResult(message) => {
            chars_to_tokens(message.content.iter().map(block_chars).sum())
        }
        Message::Assistant(message) => chars_to_tokens(
            message
                .content
                .iter()
                .map(|block| match block {
                    AssistantContent::Text(text) => text.text.len(),
                    AssistantContent::Thinking(thinking) => thinking.thinking.len(),
                    AssistantContent::ToolCall(call) => {
                        utf16_len(&call.name) + stringify_object(&call.arguments).len()
                    }
                })
                .sum(),
        ),
    }
}

fn estimate_messages(messages: &[Message]) -> ContextUsageEstimate {
    let mut latest_prefix_timestamp = f64::NEG_INFINITY;
    let mut usage_info = None;
    for (index, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message
            && assistant.timestamp >= latest_prefix_timestamp
            && !matches!(
                assistant.stop_reason,
                StopReason::Aborted | StopReason::Error
            )
            && calculate_context_tokens(&assistant.usage) > 0.0
        {
            usage_info = Some((index, calculate_context_tokens(&assistant.usage)));
        }
        let timestamp = match message {
            Message::User(message) => message.timestamp,
            Message::Assistant(message) => message.timestamp,
            Message::ToolResult(message) => message.timestamp,
        };
        latest_prefix_timestamp = latest_prefix_timestamp.max(timestamp);
    }

    if let Some((index, usage_tokens)) = usage_info {
        let trailing_tokens = messages[index + 1..]
            .iter()
            .fold(0.0, |sum, message| sum + estimate_message_tokens(message));
        return ContextUsageEstimate {
            tokens: usage_tokens + trailing_tokens,
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(index),
        };
    }

    let tokens = messages
        .iter()
        .fold(0.0, |sum, message| sum + estimate_message_tokens(message));
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0.0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

fn estimate_tools_tokens<'a>(tools: impl IntoIterator<Item = &'a Tool>) -> f64 {
    let tools = tools.into_iter().collect::<Vec<_>>();
    if tools.is_empty() {
        0.0
    } else {
        estimate_text_tokens(
            &serde_json::to_string(&tools).unwrap_or_else(|_| "[unserializable]".to_owned()),
        )
    }
}

pub fn estimate_message_array_tokens(messages: &[Message]) -> ContextUsageEstimate {
    estimate_messages(messages)
}

pub fn estimate_context_tokens(context: &Context) -> ContextUsageEstimate {
    let mut estimate = estimate_messages(&context.messages);
    if let Some(index) = estimate.last_usage_index {
        let added_names = context.messages[index + 1..]
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(message) => message.added_tool_names.as_ref(),
                Message::User(_) | Message::Assistant(_) => None,
            })
            .flatten()
            .collect::<BTreeSet<_>>();
        let added_tool_tokens = estimate_tools_tokens(
            context
                .tools
                .iter()
                .flatten()
                .filter(|tool| added_names.iter().any(|name| ***name == tool.name)),
        );
        estimate.tokens += added_tool_tokens;
        estimate.trailing_tokens += added_tool_tokens;
        return estimate;
    }

    let prefix_tokens = context
        .system_prompt
        .as_ref()
        .map_or(0.0, |text| chars_to_tokens(text.len()))
        + estimate_tools_tokens(context.tools.iter().flatten());
    estimate.tokens += prefix_tokens;
    estimate.trailing_tokens += prefix_tokens;
    estimate
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn total_usage_is_numeric_and_zero_falls_back_to_parts() {
        let mut usage = Usage {
            input: 2.0,
            output: 3.0,
            ..Usage::default()
        };
        assert_eq!(calculate_context_tokens(&usage), 5.0);
        usage.total_tokens = 9.5;
        assert_eq!(calculate_context_tokens(&usage), 9.5);
    }
}
