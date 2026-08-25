//! §10 message-overflow conformance against pinned Pi
//! `packages/ai/test/overflow.test.ts` and `context-overflow.test.ts`.

use pi_ai::{
    ApiId, AssistantFinish, AssistantFinishReason, AssistantMessage, MessageId, ModelId,
    ProviderId, PublicError, ReplayEnvelope, ReplayScope, Timestamp, Usage, UsageSource,
    is_context_overflow, is_recoverable_length,
};

fn message(
    reason: AssistantFinishReason,
    error: Option<&str>,
    input: u64,
    cache: u64,
    output: u64,
) -> AssistantMessage {
    AssistantMessage {
        id: MessageId::new("message"),
        provider: ProviderId::new("test-provider"),
        api: ApiId::new("openai-completions"),
        requested_model: ModelId::new("test-model"),
        response_model: None,
        response_id: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: Vec::new(),
        replay: ReplayEnvelope::new(ReplayScope::new(
            ProviderId::new("test-provider"),
            ApiId::new("openai-completions"),
            ModelId::new("test-model"),
            ModelId::new("test-model"),
        )),
        usage: Usage {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: None,
            cache_read_tokens: Some(cache),
            cache_write_tokens: None,
            cache_write_one_hour_tokens: None,
            total_tokens: None,
            source: UsageSource::ProviderReported,
        },
        cost: None,
        finish: AssistantFinish {
            reason,
            raw_provider_reason: None,
            error: error.map(|text| PublicError {
                code: "provider_error".into(),
                message: text.into(),
                retryable: false,
                provider_code: None,
                status: None,
                request_id: None,
            }),
        },
        timestamp: Timestamp::default(),
    }
}

#[test]
fn overflow_patterns_pi_exact() {
    // Pi basis: packages/ai/test/overflow.test.ts.
    let explicit = [
        "400 `prompt too long; exceeded max context length by 100918 tokens`",
        "400 The input (516368 tokens) is longer than the model's context length (262144 tokens).",
        "Requested token count exceeds the model's maximum context length of 131072 tokens.",
        "Input length (265330) exceeds model's maximum context length (262144).",
        "Input length 131393 exceeds the maximum allowed input length of 131040 tokens.",
        "Prompt has 256468 tokens, but the configured context size is 256000 tokens",
        "Prompt has 5,958,968 tokens, but the configured context size is 256,000 tokens",
    ];
    for error in explicit {
        assert!(
            is_context_overflow(
                &message(AssistantFinishReason::Error, Some(error), 0, 0, 0),
                Some(262_144)
            ),
            "missed Pi overflow message: {error}"
        );
    }

    for error in [
        "500 `model runner crashed unexpectedly`",
        "This model's maximum prompt length is ١٣١٠٧٢",
        "Throttling error: Too many tokens, please wait before trying again.",
        "Service unavailable: The service is temporarily unavailable.",
        "Rate limit exceeded, please retry after 30 seconds.",
        "Too many requests. Please slow down.",
    ] {
        assert!(
            !is_context_overflow(
                &message(AssistantFinishReason::Error, Some(error), 0, 0, 0),
                Some(200_000)
            ),
            "misclassified Pi non-overflow message: {error}"
        );
    }

    let xiaomi = message(AssistantFinishReason::Length, None, 58, 1_048_512, 0);
    assert!(is_context_overflow(&xiaomi, Some(1_048_576)));
    assert!(is_recoverable_length(&xiaomi, 128_000));
    let mut recoverable = message(AssistantFinishReason::Length, None, 3, 253_584, 16);
    recoverable.usage.cache_write_tokens = Some(25_554);
    assert!(is_recoverable_length(&recoverable, 128_000));
    assert!(!is_context_overflow(
        &message(AssistantFinishReason::Length, None, 1_000, 0, 4_096),
        Some(200_000)
    ));
    let zero_output = message(AssistantFinishReason::Length, None, 100, 0, 0);
    assert!(is_recoverable_length(&zero_output, 128_000));
    assert!(!is_context_overflow(&zero_output, Some(200_000)));
    assert!(!is_recoverable_length(
        &message(AssistantFinishReason::Length, None, 4_062, 0, 1_024),
        1_024
    ));
}

#[test]
fn context_overflow_classifier_pi_exact() {
    // Pi basis: packages/ai/test/context-overflow.test.ts and
    // packages/ai/src/utils/overflow.ts provider examples. The upstream test
    // is live-provider based; these captured terminal messages are hermetic.
    let error_cases = [
        (
            "Anthropic API key",
            "prompt is too long: 213462 tokens > 200000 maximum",
        ),
        (
            "Anthropic OAuth",
            "413 {\"error\":{\"type\":\"request_too_large\"}}",
        ),
        (
            "Copilot Google",
            "prompt token count of 200001 exceeds the limit of 200000",
        ),
        ("Copilot Anthropic", "Input is too long for requested model"),
        (
            "OpenAI Completions",
            "Requested token count exceeds the model's maximum context length of 131072 tokens",
        ),
        (
            "OpenAI Responses",
            "Your input exceeds the context window of this model",
        ),
        ("Azure OpenAI Responses", "context_length_exceeded"),
        (
            "Google",
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
        ),
        ("OpenAI Codex OAuth", "token limit exceeded"),
        ("Amazon Bedrock", "Input is too long for requested model"),
        (
            "xAI",
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
        ),
        (
            "Groq",
            "Please reduce the length of the messages or completion",
        ),
        ("Cerebras", "400 status code (no body)"),
        ("Hugging Face", "context_length_exceeded"),
        (
            "Together AI",
            "The input (516368 tokens) is longer than the model's context length (262144 tokens)",
        ),
        ("z.ai explicit", "model_context_window_exceeded"),
        (
            "Mistral",
            "Prompt contains 300000 tokens, too large for model with 262144 maximum context length",
        ),
        ("MiniMax", "invalid params, context window exceeds limit"),
        (
            "Qwen Token Plan",
            "Range of input length should be [1, 131072]",
        ),
        (
            "Qwen Token Plan Individual",
            "Range of input length should be [1, 131072]",
        ),
        (
            "Qwen Token Plan CN",
            "Range of input length should be [1, 131072]",
        ),
        (
            "Kimi For Coding",
            "Your request exceeded model token limit: 131072 (requested: 140000)",
        ),
        (
            "Vercel AI Gateway",
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
        ),
        (
            "OpenRouter Anthropic",
            "This endpoint's maximum context length is 128000 tokens. However, you requested about 130000 tokens",
        ),
        (
            "OpenRouter DeepSeek",
            "This endpoint's maximum context length is 128000 tokens. However, you requested about 130000 tokens",
        ),
        (
            "OpenRouter Mistral",
            "This endpoint's maximum context length is 128000 tokens. However, you requested about 130000 tokens",
        ),
        (
            "OpenRouter Google",
            "This endpoint's maximum context length is 128000 tokens. However, you requested about 130000 tokens",
        ),
        (
            "OpenRouter Meta/Llama",
            "This endpoint's maximum context length is 128000 tokens. However, you requested about 130000 tokens",
        ),
        (
            "Ollama explicit error",
            "prompt too long; exceeded max context length by 100918 tokens",
        ),
        (
            "LM Studio",
            "tokens to keep from the initial prompt is greater than the context length",
        ),
        (
            "llama.cpp",
            "the request exceeds the available context size, try increasing it",
        ),
    ];
    for (case, error) in error_cases {
        assert!(
            is_context_overflow(
                &message(AssistantFinishReason::Error, Some(error), 0, 0, 0),
                Some(200_000)
            ),
            "missed {} overflow message: {}",
            case,
            error,
        );
    }

    // Four separate pinned Xiaomi provider cases exercise the identical
    // silent length-stop heuristic.
    for provider in [
        "xiaomi",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-sgp",
    ] {
        assert!(
            is_context_overflow(
                &message(AssistantFinishReason::Length, None, 58, 1_048_512, 0),
                Some(1_048_576),
            ),
            "missed {provider} silent length overflow",
        );
    }

    // z.ai's successful-over-limit branch is separately observable from its
    // explicit error branch above.
    assert!(is_context_overflow(
        &message(AssistantFinishReason::Stop, None, 200_001, 0, 12),
        Some(200_000)
    ));
    assert!(!is_context_overflow(
        &message(AssistantFinishReason::Stop, None, 200_000, 0, 12),
        Some(200_000)
    ));
}
