//! Context-overflow detection ⇐ pi `src/utils/overflow.ts`.

use crate::types::{AssistantMessage, StopReason};
use regex::Regex;
use std::sync::OnceLock;

const OVERFLOW_SOURCES: &[&str] = &[
    "prompt is too long",
    "request_too_large",
    "input is too long for requested model",
    "exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    "input token count.*exceeds the maximum",
    r"maximum prompt length is \d+",
    "reduce the length of the messages",
    r"maximum context length is \d+ tokens",
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
    r"exceeds the limit of \d+",
    "exceeds the available context size",
    "greater than the context length",
    "context window exceeds limit",
    "exceeded model token limit",
    r"too large for model with \d+ maximum context length",
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    "model_context_window_exceeded",
    "prompt too long; exceeded (?:max )?context length",
    "range of input length should be",
    "context[_ ]length[_ ]exceeded",
    "too many tokens",
    "token limit exceeded",
    r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)",
];

fn overflow_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        OVERFLOW_SOURCES
            .iter()
            .map(|source| Regex::new(&format!("(?i){source}")).expect("static overflow regex"))
            .collect()
    })
}

fn non_overflow_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            "(?i)^(Throttling error|Service unavailable):",
            "(?i)rate limit",
            "(?i)too many requests",
        ]
        .into_iter()
        .map(|source| Regex::new(source).expect("static non-overflow regex"))
        .collect()
    })
}

pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<f64>) -> bool {
    if message.stop_reason == StopReason::Error
        && let Some(error) = message.error_message.as_deref()
        && !non_overflow_patterns()
            .iter()
            .any(|pattern| pattern.is_match(error))
        && overflow_patterns()
            .iter()
            .any(|pattern| pattern.is_match(error))
    {
        return true;
    }
    if let Some(window) = context_window.filter(|window| *window != 0.0) {
        let input = message
            .usage
            .input
            .js_add(&message.usage.cache_read)
            .as_number();
        if message.stop_reason == StopReason::Stop && input > window {
            return true;
        }
        if message.stop_reason == StopReason::Length
            && message.usage.output == 0
            && input >= window * 0.99
        {
            return true;
        }
    }
    false
}

pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: f64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0.0
        && message.usage.output.as_number() < desired_max_output
}

pub fn get_overflow_patterns() -> Vec<Regex> {
    overflow_patterns().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, StopReason};

    fn message(
        reason: StopReason,
        error: Option<&str>,
        input: f64,
        cache: f64,
        output: f64,
    ) -> AssistantMessage {
        let mut message = AssistantMessage::pending("openai-completions", "test", "model", 1);
        message.stop_reason = reason;
        message.error_message = error.map(str::to_owned);
        message.usage.input = input.into();
        message.usage.cache_read = cache.into();
        message.usage.output = output.into();
        message.usage.total_tokens = (input + cache + output).into();
        message
    }

    /// Ports pi `test/overflow.test.ts:32-179`.
    #[test]
    fn ports_context_overflow_and_recoverable_length_matrix() {
        for error in [
            "400 `prompt too long; exceeded max context length by 100918 tokens`",
            "400 The input (516368 tokens) is longer than the model's context length (262144 tokens).",
            "Requested token count exceeds the model's maximum context length of 131072 tokens.",
            "Input length (265330) exceeds model's maximum context length (262144).",
            "Provider returned error: Input length 131393 exceeds the maximum allowed input length of 131040 tokens.",
            "Prompt has 5,958,968 tokens, but the configured context size is 256,000 tokens",
        ] {
            assert!(is_context_overflow(
                &message(StopReason::Error, Some(error), 0.0, 0.0, 0.0),
                Some(262_144.0)
            ));
        }
        for error in [
            "500 `model runner crashed unexpectedly`",
            "Throttling error: Too many tokens, please wait before trying again.",
            "Service unavailable: The service is temporarily unavailable.",
            "Rate limit exceeded, please retry after 30 seconds.",
            "Too many requests. Please slow down.",
        ] {
            assert!(!is_context_overflow(
                &message(StopReason::Error, Some(error), 0.0, 0.0, 0.0),
                Some(200_000.0)
            ));
        }

        let filled = message(StopReason::Length, None, 58.0, 1_048_512.0, 0.0);
        assert!(is_context_overflow(&filled, Some(1_048_576.0)));
        assert!(is_recoverable_length(&filled, 128_000.0));

        let reached = message(StopReason::Length, None, 4_062.0, 0.0, 1_024.0);
        assert!(!is_recoverable_length(&reached, 1_024.0));
        assert!(!is_context_overflow(&reached, Some(200_000.0)));

        let far_below = message(StopReason::Length, None, 100.0, 0.0, 0.0);
        assert!(is_recoverable_length(&far_below, 128_000.0));
        assert!(!is_context_overflow(&far_below, Some(200_000.0)));

        let mut off_spec = message(StopReason::Length, None, 200_000.0, 0.0, 0.0);
        off_spec.usage.output = serde_json::json!("0").into();
        assert!(!is_context_overflow(&off_spec, Some(200_000.0)));
    }
}
