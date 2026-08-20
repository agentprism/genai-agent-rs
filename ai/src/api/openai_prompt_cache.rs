//! OpenAI prompt-cache key handling ⇐ pi `src/api/openai-prompt-cache.ts`.

pub const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

pub fn clamp_open_ai_prompt_cache_key(key: Option<&str>) -> Option<String> {
    key.map(|key| {
        key.chars()
            .take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Derived from pi `src/api/openai-prompt-cache.ts:3-8`.
    #[test]
    fn clamps_by_unicode_code_point_without_splitting_emoji() {
        assert_eq!(clamp_open_ai_prompt_cache_key(None), None);
        assert_eq!(
            clamp_open_ai_prompt_cache_key(Some("short")).as_deref(),
            Some("short")
        );
        let key = format!("{}tail", "🙈".repeat(64));
        assert_eq!(
            clamp_open_ai_prompt_cache_key(Some(&key)),
            Some("🙈".repeat(64))
        );
    }
}
