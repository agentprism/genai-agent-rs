//! Message-level context-overflow classification from pinned Pi
//! `packages/ai/src/utils/overflow.ts` (architecture v2 part 2 §10).

use crate::{AssistantFinishReason, AssistantMessage};
use regex::Regex;
use std::sync::LazyLock;

/// Provider and proxy error-message patterns which indicate input overflow.
///
/// Pattern order and matching behavior follow pinned Pi. Callers should use
/// [`is_context_overflow`] so the non-overflow exclusions retain precedence.
pub static OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"prompt is too long",
        r"request_too_large",
        r"input is too long for requested model",
        r"exceeds the context window",
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [0-9,]+ tokens?|\s*\([0-9,]+\))",
        r"input token count.*exceeds the maximum",
        r"maximum prompt length is [0-9]+",
        r"reduce the length of the messages",
        r"maximum context length is [0-9]+ tokens",
        r"exceeds (?:the )?maximum allowed input length of [0-9,]+ tokens?",
        r"input \([0-9]+ tokens\) is longer than the model'?s context length \([0-9]+ tokens\)",
        r"exceeds the limit of [0-9]+",
        r"exceeds the available context size",
        r"greater than the context length",
        r"context window exceeds limit",
        r"exceeded model token limit",
        r"too large for model with [0-9]+ maximum context length",
        r"prompt has [0-9,]+ tokens?, but the configured context size is [0-9,]+ tokens?",
        r"model_context_window_exceeded",
        r"prompt too long; exceeded (?:max )?context length",
        r"range of input length should be",
        r"context[_ ]length[_ ]exceeded",
        r"too many tokens",
        r"token limit exceeded",
        r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)",
    ]
    .into_iter()
    .map(|pattern| Regex::new(&format!("(?i){pattern}")).expect("Pi overflow regex is valid"))
    .collect()
});

/// Error-message patterns which take precedence over overflow matches.
///
/// These exclusions prevent throttling and service failures containing text
/// such as "too many tokens" from being classified as context overflow.
pub static NON_OVERFLOW_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^(Throttling error|Service unavailable):",
        r"rate limit",
        r"too many requests",
    ]
    .into_iter()
    .map(|pattern| Regex::new(&format!("(?i){pattern}")).expect("Pi exclusion regex is valid"))
    .collect()
});

/// Returns whether a terminal assistant message represents context overflow.
///
/// This ports pinned Pi's three ordered checks: explicit provider error text,
/// successful silent overflow, and Xiaomi-style zero-output length stops.
/// `context_window` is optional because explicit errors need no catalog data.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    if message.finish.reason == AssistantFinishReason::Error
        && let Some(error) = &message.finish.error
    {
        let is_non_overflow = NON_OVERFLOW_PATTERNS
            .iter()
            .any(|pattern| pattern.is_match(&error.message));
        if !is_non_overflow
            && OVERFLOW_PATTERNS
                .iter()
                .any(|pattern| pattern.is_match(&error.message))
        {
            return true;
        }
    }

    let Some(context_window) = context_window.filter(|window| *window != 0) else {
        return false;
    };
    let input_tokens = u128::from(message.usage.input_tokens)
        + u128::from(message.usage.cache_read_tokens.unwrap_or(0));

    if message.finish.reason == AssistantFinishReason::Stop
        && input_tokens > u128::from(context_window)
    {
        return true;
    }

    message.finish.reason == AssistantFinishReason::Length
        && message.usage.output_tokens == 0
        && input_tokens.saturating_mul(100) >= u128::from(context_window).saturating_mul(99)
}

/// Returns whether a length stop ended below the intended output limit.
///
/// Such a response may be caused by context pressure or provider truncation,
/// so a caller can make one bounded compact-and-retry attempt.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.finish.reason == AssistantFinishReason::Length
        && desired_max_output > 0
        && message.usage.output_tokens < desired_max_output
}
