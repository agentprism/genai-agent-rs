//! Context-token estimates ⇐ pi `src/utils/estimate.ts`.

use crate::types::{
    AssistantContent, Context, Message, StopReason, Tool, Usage, UsageValue, UserContent,
    UserContentBlock,
};
use std::collections::BTreeSet;

const CHARS_PER_TOKEN: usize = 4;
const ESTIMATED_IMAGE_CHARS: usize = 4_800;

#[derive(Debug, Clone, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: UsageValue,
    pub usage_tokens: UsageValue,
    pub trailing_tokens: UsageValue,
    pub last_usage_index: Option<usize>,
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn chars_to_tokens(chars: usize) -> f64 {
    chars.div_ceil(CHARS_PER_TOKEN) as f64
}

pub fn calculate_context_tokens(usage: &Usage) -> UsageValue {
    if usage.total_tokens.is_truthy() {
        usage.total_tokens.clone()
    } else {
        usage
            .input
            .js_add(&usage.output)
            .js_add(&usage.cache_read)
            .js_add(&usage.cache_write)
    }
}

pub fn estimate_text_tokens(text: &str) -> f64 {
    chars_to_tokens(utf16_len(text))
}

fn content_chars(content: &UserContent) -> usize {
    match content {
        UserContent::Text(text) => utf16_len(text),
        UserContent::Blocks(blocks) => blocks.iter().map(block_chars).sum(),
    }
}

fn block_chars(block: &UserContentBlock) -> usize {
    match block {
        UserContentBlock::Text(text) => utf16_len(&text.text),
        UserContentBlock::Image(_) => ESTIMATED_IMAGE_CHARS,
        UserContentBlock::Unknown(_) => 0,
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
                    AssistantContent::Text(text) => utf16_len(&text.text),
                    AssistantContent::Thinking(thinking) => utf16_len(&thinking.thinking),
                    AssistantContent::ToolCall(call) => {
                        utf16_len(&call.name)
                            + utf16_len(
                                &serde_json::to_string(&call.arguments)
                                    .unwrap_or_else(|_| "[unserializable]".to_owned()),
                            )
                    }
                    AssistantContent::Unknown(_) => 0,
                })
                .sum(),
        ),
    }
}

fn estimate_messages(messages: &[Message]) -> ContextUsageEstimate {
    let mut latest_prefix_timestamp = i64::MIN;
    let mut usage_info = None;
    for (index, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message
            && assistant.timestamp >= latest_prefix_timestamp
            && !matches!(
                assistant.stop_reason,
                StopReason::Aborted | StopReason::Error
            )
            && calculate_context_tokens(&assistant.usage).as_number() > 0.0
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
            tokens: usage_tokens.js_add(&trailing_tokens.into()),
            usage_tokens,
            trailing_tokens: trailing_tokens.into(),
            last_usage_index: Some(index),
        };
    }

    let tokens = messages
        .iter()
        .fold(0.0, |sum, message| sum + estimate_message_tokens(message));
    ContextUsageEstimate {
        tokens: tokens.into(),
        usage_tokens: 0.0.into(),
        trailing_tokens: tokens.into(),
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
                .filter(|tool| added_names.contains(&tool.name)),
        );
        estimate.tokens = estimate.tokens.js_add(&added_tool_tokens.into());
        estimate.trailing_tokens = estimate.trailing_tokens.js_add(&added_tool_tokens.into());
        return estimate;
    }

    let prefix_tokens = context
        .system_prompt
        .as_deref()
        .map_or(0.0, estimate_text_tokens)
        + estimate_tools_tokens(context.tools.iter().flatten());
    estimate.tokens = estimate.tokens.js_add(&prefix_tokens.into());
    estimate.trailing_tokens = estimate.trailing_tokens.js_add(&prefix_tokens.into());
    estimate
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pins pi `src/utils/estimate.ts:17-18,92-97` JavaScript `||` and `+`
    /// behavior when provider usage is off-spec but truthy.
    #[test]
    fn truthy_string_usage_remains_observable() {
        let mut message = crate::types::AssistantMessage::pending("api", "provider", "model", 1);
        message.stop_reason = StopReason::Stop;
        message.usage.total_tokens = json!("5").into();
        let trailing = crate::types::UserMessage {
            role: crate::types::UserRole::User,
            content: UserContent::Text("tail".to_owned()),
            timestamp: 2,
        };
        let estimate = estimate_message_array_tokens(&[
            Message::Assistant(Box::new(message)),
            Message::User(Box::new(trailing)),
        ]);
        assert_eq!(
            serde_json::to_value(estimate.usage_tokens).unwrap(),
            json!("5")
        );
        assert_eq!(serde_json::to_value(estimate.tokens).unwrap(), json!("51"));
    }
}
