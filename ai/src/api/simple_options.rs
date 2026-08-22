//! Provider-neutral option lowering ⇐ pi `src/api/simple-options.ts`.

use crate::types::{
    Context, Model, SimpleStreamOptions, StreamOptions, ThinkingBudgets, ThinkingLevel,
};
use crate::utils::estimate::estimate_context_tokens;

const CONTEXT_SAFETY_TOKENS: f64 = 4_096.0;
const MIN_MAX_TOKENS: f64 = 1.0;
pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: f64) -> f64 {
    if model.context_window == 0.0 {
        return MIN_MAX_TOKENS.max(max_tokens);
    }
    let available =
        model.context_window - estimate_context_tokens(context).tokens - CONTEXT_SAFETY_TOKENS;
    max_tokens.min(available.max(MIN_MAX_TOKENS))
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

pub const MIN_ANSWER_TOKENS: f64 = 1_024.0;

pub const DEFAULT_THINKING_BUDGETS: ThinkingBudgets = ThinkingBudgets {
    minimal: Some(1_024.0),
    low: Some(2_048.0),
    medium: Some(8_192.0),
    high: Some(16_384.0),
};

pub fn default_thinking_budgets() -> ThinkingBudgets {
    DEFAULT_THINKING_BUDGETS.clone()
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
) -> f64 {
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

pub fn clamp_thinking_budget_to_answer_room(thinking_budget: f64, ceiling: f64) -> f64 {
    thinking_budget.min((ceiling - MIN_ANSWER_TOKENS).max(0.0))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThinkingTokenAdjustment {
    pub max_tokens: f64,
    pub thinking_budget: f64,
}

pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<f64>,
    model_max_tokens: f64,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> ThinkingTokenAdjustment {
    let mut thinking_budget = thinking_budget_for_level(reasoning_level, custom_budgets);
    let max_tokens = base_max_tokens.map_or(model_max_tokens, |base| {
        (base + thinking_budget).min(model_max_tokens)
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
            context_window: 10_000.0,
            max_tokens: 8_000.0,
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
            AssistantMessage::pending("openai-responses", "openai", "test", timestamp as f64);
        message.content = vec![AssistantContent::Text(TextContent::new("kept"))];
        message.stop_reason = StopReason::Stop;
        message.usage.input = total_tokens as f64;
        message.usage.total_tokens = total_tokens as f64;
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
        options.stream.max_tokens = Some(0.0);
        options.stream.request.api_key = Some(String::new());
        options.stream.sampling_params = Some(Map::from_iter([
            ("seed".to_owned(), json!(0)),
            ("custom".to_owned(), Value::Null),
        ]));
        options.stream.metadata = Some(Map::new());
        options.stream.request.headers = Some(crate::types::ProviderHeaders::from([(
            "x".to_owned(),
            None,
        )]));
        let telemetry: Arc<dyn TelemetryContext> = Arc::new(TestTelemetry);
        options.stream.request.telemetry_context = Some(Arc::clone(&telemetry));
        let built = build_base_options(&model, &context, Some(&options), Some("resolved"));
        assert_eq!(built.temperature, Some(0.0));
        assert_eq!(built.max_tokens, Some(0.0));
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
            1_024.0
        );
        assert_eq!(
            thinking_budget_for_level(ThinkingLevel::Max, None),
            16_384.0
        );
        let custom = ThinkingBudgets {
            high: Some(20_000.0),
            ..ThinkingBudgets::default()
        };
        assert_eq!(
            thinking_budget_for_level(ThinkingLevel::Xhigh, Some(&custom)),
            20_000.0
        );
        assert_eq!(
            clamp_thinking_budget_to_answer_room(8_192.0, 5_000.0),
            3_976.0
        );
        assert_eq!(clamp_thinking_budget_to_answer_room(1_024.0, 1_000.0), 0.0);
        assert_eq!(
            adjust_max_tokens_for_thinking(None, 4_096.0, ThinkingLevel::High, None),
            ThinkingTokenAdjustment {
                max_tokens: 4_096.0,
                thinking_budget: 3_072.0,
            }
        );
        assert_eq!(
            adjust_max_tokens_for_thinking(Some(1_000.0), 20_000.0, ThinkingLevel::Low, None),
            ThinkingTokenAdjustment {
                max_tokens: 3_048.0,
                thinking_budget: 2_048.0,
            }
        );
    }

    /// Pins the context clamp used by pi `buildBaseOptions` (`src/api/simple-options.ts:15-19`).
    #[test]
    fn clamps_max_tokens_to_context_safety_room() {
        let mut model = model();
        model.context_window = 10_000.0;
        model.max_tokens = 8_000.0;
        let context = Context {
            system_prompt: Some(("x".repeat(4_000)).into()),
            messages: vec![Message::User(Box::new(UserMessage {
                role: UserRole::User,
                content: UserContent::Text(("hello world".to_owned()).into()),
                timestamp: 1.0,
            }))],
            tools: None,
        };
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context, 8_000.0),
            4_901.0
        );
    }

    /// Ports pi `test/context-estimate.test.ts:43-80`, used by `buildBaseOptions`.
    #[test]
    fn context_usage_applies_only_to_the_prefix_it_describes() {
        let mut model = model();
        model.sampling_params = None;
        let stale_context = Context {
            system_prompt: Some(("system".to_owned()).into()),
            messages: vec![
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text(("summary".to_owned()).into()),
                    timestamp: 200.0,
                })),
                assistant(100, 9_500),
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text(("x".repeat(4_000)).into()),
                    timestamp: 300.0,
                })),
            ],
            tools: None,
        };
        assert_eq!(estimate_context_tokens(&stale_context).tokens, 1_005.0);
        assert_eq!(
            build_base_options(&model, &stale_context, None, None).max_tokens,
            Some(4_899.0)
        );

        let current_context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text(("summary".to_owned()).into()),
                    timestamp: 200.0,
                })),
                assistant(100, 9_500),
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text(("new prompt".to_owned()).into()),
                    timestamp: 300.0,
                })),
                assistant(400, 2_000),
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text(("tail".to_owned()).into()),
                    timestamp: 500.0,
                })),
            ],
            tools: None,
        };
        assert_eq!(estimate_context_tokens(&current_context).tokens, 2_001.0);
    }
}
