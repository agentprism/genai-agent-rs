//! Provider-neutral option lowering ⇐ pi `src/api/simple-options.ts`.

use crate::types::{
    AssistantContent, Context, Message, Model, SimpleStreamOptions, StopReason, StreamOptions,
    ThinkingBudgets, ThinkingLevel, UserContent, UserContentBlock,
};
use std::collections::BTreeSet;

const CONTEXT_SAFETY_TOKENS: u64 = 4_096;
const MIN_MAX_TOKENS: u64 = 1;
const CHARS_PER_TOKEN: usize = 4;
const ESTIMATED_IMAGE_CHARS: usize = 4_800;

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn estimate_content_chars(content: &UserContent) -> usize {
    match content {
        UserContent::Text(text) => utf16_len(text),
        UserContent::Blocks(blocks) => blocks.iter().map(estimate_block_chars).sum(),
    }
}

fn estimate_block_chars(block: &UserContentBlock) -> usize {
    match block {
        UserContentBlock::Text(text) => utf16_len(&text.text),
        UserContentBlock::Image(_) => ESTIMATED_IMAGE_CHARS,
    }
}

fn chars_to_tokens(chars: usize) -> u64 {
    u64::try_from(chars.div_ceil(CHARS_PER_TOKEN)).unwrap_or(u64::MAX)
}

fn calculate_context_tokens(usage: &crate::types::Usage) -> u64 {
    if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
    }
}

fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::User(message) => chars_to_tokens(estimate_content_chars(&message.content)),
        Message::ToolResult(message) => chars_to_tokens(
            message
                .content
                .iter()
                .map(estimate_block_chars)
                .sum::<usize>(),
        ),
        Message::Assistant(message) => {
            let chars = message
                .content
                .iter()
                .map(|block| match block {
                    AssistantContent::Text(block) => utf16_len(&block.text),
                    AssistantContent::Thinking(block) => utf16_len(&block.thinking),
                    AssistantContent::ToolCall(block) => {
                        let arguments = serde_json::to_string(&block.arguments)
                            .unwrap_or_else(|_| "[unserializable]".to_owned());
                        utf16_len(&block.name) + utf16_len(&arguments)
                    }
                })
                .sum();
            chars_to_tokens(chars)
        }
    }
}

fn estimate_tools_tokens<'a>(tools: impl IntoIterator<Item = &'a crate::types::Tool>) -> u64 {
    let tools = tools.into_iter().collect::<Vec<_>>();
    if tools.is_empty() {
        return 0;
    }
    let json = serde_json::to_string(&tools).unwrap_or_else(|_| "[unserializable]".to_owned());
    chars_to_tokens(utf16_len(&json))
}

fn estimate_context_tokens(context: &Context) -> u64 {
    let mut latest_prefix_timestamp: Option<i64> = None;
    let mut usage_info = None;
    for (index, message) in context.messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            let applies =
                latest_prefix_timestamp.is_none_or(|timestamp| assistant.timestamp >= timestamp);
            if applies
                && !matches!(
                    assistant.stop_reason,
                    StopReason::Aborted | StopReason::Error
                )
                && calculate_context_tokens(&assistant.usage) > 0
            {
                usage_info = Some((index, calculate_context_tokens(&assistant.usage)));
            }
        }
        let timestamp = match message {
            Message::User(message) => message.timestamp,
            Message::Assistant(message) => message.timestamp,
            Message::ToolResult(message) => message.timestamp,
        };
        latest_prefix_timestamp =
            Some(latest_prefix_timestamp.map_or(timestamp, |latest| latest.max(timestamp)));
    }

    if let Some((index, usage_tokens)) = usage_info {
        let trailing = context.messages[index + 1..]
            .iter()
            .fold(0_u64, |total, message| {
                total.saturating_add(estimate_message_tokens(message))
            });
        let added_names = context.messages[index + 1..]
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(message) => message.added_tool_names.as_ref(),
                Message::User(_) | Message::Assistant(_) => None,
            })
            .flatten()
            .collect::<BTreeSet<_>>();
        let added_tools = estimate_tools_tokens(
            context
                .tools
                .iter()
                .flatten()
                .filter(|tool| added_names.contains(&tool.name)),
        );
        return usage_tokens
            .saturating_add(trailing)
            .saturating_add(added_tools);
    }

    let messages = context.messages.iter().fold(0_u64, |total, message| {
        total.saturating_add(estimate_message_tokens(message))
    });
    let system = context
        .system_prompt
        .as_deref()
        .map_or(0, |prompt| chars_to_tokens(utf16_len(prompt)));
    let tools = estimate_tools_tokens(context.tools.iter().flatten());
    messages.saturating_add(system).saturating_add(tools)
}

pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u64) -> u64 {
    if model.context_window == 0 {
        return MIN_MAX_TOKENS.max(max_tokens);
    }
    let available = i128::from(model.context_window)
        - i128::from(estimate_context_tokens(context))
        - i128::from(CONTEXT_SAFETY_TOKENS);
    let available = u64::try_from(available.max(i128::from(MIN_MAX_TOKENS))).unwrap_or(u64::MAX);
    max_tokens.min(available)
}

pub fn build_base_options(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
    api_key: Option<&str>,
) -> StreamOptions {
    let mut result = options.map_or_else(StreamOptions::default, |options| options.stream.clone());
    result.sampling_params = if model.sampling_params.is_some()
        || options
            .and_then(|options| options.stream.sampling_params.as_ref())
            .is_some()
    {
        let mut parameters = model.sampling_params.clone().unwrap_or_default();
        if let Some(overrides) = options.and_then(|options| options.stream.sampling_params.as_ref())
        {
            parameters.extend(overrides.clone());
        }
        Some(parameters)
    } else {
        None
    };
    let requested = options
        .and_then(|options| options.stream.max_tokens)
        .unwrap_or(model.max_tokens);
    result.max_tokens = Some(clamp_max_tokens_to_context(model, context, requested));
    if let Some(api_key) = api_key.filter(|api_key| !api_key.is_empty()) {
        result.request.api_key = Some(api_key.to_owned());
    }
    result
}

pub const MIN_ANSWER_TOKENS: u64 = 1_024;

pub fn default_thinking_budgets() -> ThinkingBudgets {
    ThinkingBudgets {
        minimal: Some(1_024),
        low: Some(2_048),
        medium: Some(8_192),
        high: Some(16_384),
    }
}

pub fn clamp_reasoning(effort: Option<ThinkingLevel>) -> Option<ThinkingLevel> {
    match effort {
        Some(ThinkingLevel::Xhigh | ThinkingLevel::Max) => Some(ThinkingLevel::High),
        value => value,
    }
}

pub fn thinking_budget_for_level(
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> u64 {
    let defaults = default_thinking_budgets();
    let level = clamp_reasoning(Some(reasoning_level)).expect("reasoning level is present");
    let custom = custom_budgets.and_then(|budgets| match level {
        ThinkingLevel::Minimal => budgets.minimal,
        ThinkingLevel::Low => budgets.low,
        ThinkingLevel::Medium => budgets.medium,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => budgets.high,
    });
    custom.unwrap_or_else(|| match level {
        ThinkingLevel::Minimal => defaults.minimal.expect("default"),
        ThinkingLevel::Low => defaults.low.expect("default"),
        ThinkingLevel::Medium => defaults.medium.expect("default"),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => {
            defaults.high.expect("default")
        }
    })
}

pub fn clamp_thinking_budget_to_answer_room(thinking_budget: u64, ceiling: u64) -> u64 {
    thinking_budget.min(ceiling.saturating_sub(MIN_ANSWER_TOKENS))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingTokenAdjustment {
    pub max_tokens: u64,
    pub thinking_budget: u64,
}

pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> ThinkingTokenAdjustment {
    let mut thinking_budget = thinking_budget_for_level(reasoning_level, custom_budgets);
    let max_tokens = base_max_tokens.map_or(model_max_tokens, |base| {
        base.saturating_add(thinking_budget).min(model_max_tokens)
    });
    if max_tokens <= thinking_budget {
        thinking_budget = clamp_thinking_budget_to_answer_room(thinking_budget, max_tokens);
    }
    ThinkingTokenAdjustment {
        max_tokens,
        thinking_budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use serde_json::{Map, Value, json};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[derive(Debug)]
    struct TestTelemetry;

    impl TelemetryContext for TestTelemetry {}

    fn model() -> Model {
        Model {
            id: "test".to_owned(),
            name: "Test".to_owned(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            base_url: "https://example.test".to_owned(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 10_000,
            max_tokens: 8_000,
            sampling_params: Some(Map::from_iter([
                ("top_p".to_owned(), json!(0.9)),
                ("seed".to_owned(), json!(1)),
            ])),
            headers: None,
            compat: None,
        }
    }

    fn assistant(timestamp: i64, total_tokens: u64) -> Message {
        let mut message =
            AssistantMessage::pending("openai-responses", "openai", "test", timestamp);
        message.content = vec![AssistantContent::Text(TextContent::new("kept"))];
        message.stop_reason = StopReason::Stop;
        message.usage.input = total_tokens;
        message.usage.total_tokens = total_tokens;
        Message::Assistant(Box::new(message))
    }

    /// Derived from pi `src/api/simple-options.ts:21-51`; pins field presence and precedence.
    #[test]
    fn base_options_merge_sampling_and_preserve_explicit_zero_and_empty() {
        let model = model();
        let context = Context {
            system_prompt: None,
            messages: vec![],
            tools: None,
        };
        let mut options = SimpleStreamOptions::default();
        options.stream.temperature = Some(0.0);
        options.stream.max_tokens = Some(0);
        options.stream.request.api_key = Some(String::new());
        options.stream.sampling_params = Some(Map::from_iter([
            ("seed".to_owned(), json!(0)),
            ("custom".to_owned(), Value::Null),
        ]));
        options.stream.metadata = Some(Map::new());
        options.stream.request.headers = Some(BTreeMap::from([("x".to_owned(), None)]));
        let telemetry: Arc<dyn TelemetryContext> = Arc::new(TestTelemetry);
        options.stream.request.telemetry_context = Some(Arc::clone(&telemetry));
        let built = build_base_options(&model, &context, Some(&options), Some("resolved"));
        assert_eq!(built.temperature, Some(0.0));
        assert_eq!(built.max_tokens, Some(0));
        assert_eq!(built.request.api_key.as_deref(), Some("resolved"));
        assert_eq!(
            built.sampling_params.as_ref().expect("sampling")["top_p"],
            json!(0.9)
        );
        assert_eq!(
            built.sampling_params.as_ref().expect("sampling")["seed"],
            json!(0)
        );
        assert_eq!(
            built.sampling_params.as_ref().expect("sampling")["custom"],
            Value::Null
        );
        assert_eq!(built.metadata, Some(Map::new()));
        assert_eq!(built.request.headers, options.stream.request.headers);
        assert!(Arc::ptr_eq(
            built.request.telemetry_context.as_ref().expect("telemetry"),
            &telemetry
        ));
    }

    /// Derived from pi `src/api/simple-options.ts:54-95`.
    #[test]
    fn thinking_budgets_clamp_and_adjust_exactly() {
        assert_eq!(
            thinking_budget_for_level(ThinkingLevel::Minimal, None),
            1_024
        );
        assert_eq!(thinking_budget_for_level(ThinkingLevel::Max, None), 16_384);
        let custom = ThinkingBudgets {
            high: Some(20_000),
            ..ThinkingBudgets::default()
        };
        assert_eq!(
            thinking_budget_for_level(ThinkingLevel::Xhigh, Some(&custom)),
            20_000
        );
        assert_eq!(clamp_thinking_budget_to_answer_room(8_192, 5_000), 3_976);
        assert_eq!(clamp_thinking_budget_to_answer_room(1_024, 1_000), 0);
        assert_eq!(
            adjust_max_tokens_for_thinking(None, 4_096, ThinkingLevel::High, None),
            ThinkingTokenAdjustment {
                max_tokens: 4_096,
                thinking_budget: 3_072,
            }
        );
        assert_eq!(
            adjust_max_tokens_for_thinking(Some(1_000), 20_000, ThinkingLevel::Low, None),
            ThinkingTokenAdjustment {
                max_tokens: 3_048,
                thinking_budget: 2_048,
            }
        );
    }

    /// Pins the context clamp used by pi `buildBaseOptions` (`src/api/simple-options.ts:15-19`).
    #[test]
    fn clamps_max_tokens_to_context_safety_room() {
        let mut model = model();
        model.context_window = 10_000;
        model.max_tokens = 8_000;
        let context = Context {
            system_prompt: Some("x".repeat(4_000)),
            messages: vec![Message::User(Box::new(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hello world".to_owned()),
                timestamp: 1,
            }))],
            tools: None,
        };
        assert_eq!(clamp_max_tokens_to_context(&model, &context, 8_000), 4_901);
    }

    /// Ports pi `test/context-estimate.test.ts:43-80`, used by `buildBaseOptions`.
    #[test]
    fn context_usage_applies_only_to_the_prefix_it_describes() {
        let mut model = model();
        model.sampling_params = None;
        let stale_context = Context {
            system_prompt: Some("system".to_owned()),
            messages: vec![
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text("summary".to_owned()),
                    timestamp: 200,
                })),
                assistant(100, 9_500),
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text("x".repeat(4_000)),
                    timestamp: 300,
                })),
            ],
            tools: None,
        };
        assert_eq!(estimate_context_tokens(&stale_context), 1_005);
        assert_eq!(
            build_base_options(&model, &stale_context, None, None).max_tokens,
            Some(4_899)
        );

        let current_context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text("summary".to_owned()),
                    timestamp: 200,
                })),
                assistant(100, 9_500),
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text("new prompt".to_owned()),
                    timestamp: 300,
                })),
                assistant(400, 2_000),
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text("tail".to_owned()),
                    timestamp: 500,
                })),
            ],
            tools: None,
        };
        assert_eq!(estimate_context_tokens(&current_context), 2_001);
    }
}
