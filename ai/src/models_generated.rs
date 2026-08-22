use crate::model_catalog::ModelCatalog;
use crate::providers::*;
use indexmap::IndexMap;
use std::sync::LazyLock;

pub static MODELS: LazyLock<IndexMap<&'static str, &'static ModelCatalog>> = LazyLock::new(|| {
    IndexMap::from([
        (
            "amazon-bedrock",
            &*amazon_bedrock_models::AMAZON_BEDROCK_MODELS,
        ),
        ("ant-ling", &*ant_ling_models::ANT_LING_MODELS),
        ("anthropic", &*anthropic_models::ANTHROPIC_MODELS),
        (
            "azure-openai-responses",
            &*azure_openai_responses_models::AZURE_OPENAI_RESPONSES_MODELS,
        ),
        ("baseten", &*baseten_models::BASETEN_MODELS),
        ("cerebras", &*cerebras_models::CEREBRAS_MODELS),
        (
            "cloudflare-ai-gateway",
            &*cloudflare_ai_gateway_models::CLOUDFLARE_AI_GATEWAY_MODELS,
        ),
        (
            "cloudflare-workers-ai",
            &*cloudflare_workers_ai_models::CLOUDFLARE_WORKERS_AI_MODELS,
        ),
        ("deepseek", &*deepseek_models::DEEPSEEK_MODELS),
        ("fireworks", &*fireworks_models::FIREWORKS_MODELS),
        (
            "github-copilot",
            &*github_copilot_models::GITHUB_COPILOT_MODELS,
        ),
        ("google", &*google_models::GOOGLE_MODELS),
        (
            "google-vertex",
            &*google_vertex_models::GOOGLE_VERTEX_MODELS,
        ),
        ("groq", &*groq_models::GROQ_MODELS),
        ("huggingface", &*huggingface_models::HUGGINGFACE_MODELS),
        ("kimi-coding", &*kimi_coding_models::KIMI_CODING_MODELS),
        ("minimax", &*minimax_models::MINIMAX_MODELS),
        ("minimax-cn", &*minimax_cn_models::MINIMAX_CN_MODELS),
        ("mistral", &*mistral_models::MISTRAL_MODELS),
        ("moonshotai", &*moonshotai_models::MOONSHOTAI_MODELS),
        (
            "moonshotai-cn",
            &*moonshotai_cn_models::MOONSHOTAI_CN_MODELS,
        ),
        ("nvidia", &*nvidia_models::NVIDIA_MODELS),
        ("openai", &*openai_models::OPENAI_MODELS),
        ("openai-codex", &*openai_codex_models::OPENAI_CODEX_MODELS),
        ("opencode", &*opencode_models::OPENCODE_MODELS),
        ("opencode-go", &*opencode_go_models::OPENCODE_GO_MODELS),
        ("openrouter", &*openrouter_models::OPENROUTER_MODELS),
        (
            "qwen-token-plan",
            &*qwen_token_plan_models::QWEN_TOKEN_PLAN_MODELS,
        ),
        (
            "qwen-token-plan-cn",
            &*qwen_token_plan_cn_models::QWEN_TOKEN_PLAN_CN_MODELS,
        ),
        (
            "qwen-token-plan-individual",
            &*qwen_token_plan_individual_models::QWEN_TOKEN_PLAN_INDIVIDUAL_MODELS,
        ),
        ("together", &*together_models::TOGETHER_MODELS),
        (
            "vercel-ai-gateway",
            &*vercel_ai_gateway_models::VERCEL_AI_GATEWAY_MODELS,
        ),
        ("xai", &*xai_models::XAI_MODELS),
        ("xiaomi", &*xiaomi_models::XIAOMI_MODELS),
        (
            "xiaomi-token-plan-ams",
            &*xiaomi_token_plan_ams_models::XIAOMI_TOKEN_PLAN_AMS_MODELS,
        ),
        (
            "xiaomi-token-plan-cn",
            &*xiaomi_token_plan_cn_models::XIAOMI_TOKEN_PLAN_CN_MODELS,
        ),
        (
            "xiaomi-token-plan-sgp",
            &*xiaomi_token_plan_sgp_models::XIAOMI_TOKEN_PLAN_SGP_MODELS,
        ),
        ("zai", &*zai_models::ZAI_MODELS),
        (
            "zai-coding-cn",
            &*zai_coding_cn_models::ZAI_CODING_CN_MODELS,
        ),
    ])
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::get_supported_thinking_levels;
    use crate::providers::data::{MODEL_DATA_MANIFEST, model_data_generated_at};
    use crate::types::{Api, CacheControlFormat, ModelInput, ModelThinkingLevel};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn model(provider: &str, id: &str) -> &'static crate::types::Model {
        MODELS[provider]
            .get(id)
            .unwrap_or_else(|| panic!("missing {provider}/{id}"))
    }

    fn compat_as<T: serde::de::DeserializeOwned>(provider: &str, id: &str) -> T {
        serde_json::from_value(
            serde_json::to_value(model(provider, id).compat.as_ref().expect("compat"))
                .expect("compat value"),
        )
        .expect("typed compat")
    }

    /// Ports pi `test/model-data-validation.test.ts:93-177` for the checked-in shards.
    #[test]
    fn embedded_catalog_manifest_and_api_groups_are_exact() {
        assert_eq!(MODEL_DATA_MANIFEST.schema_version, 3);
        assert!(model_data_generated_at().is_some());
        assert_eq!(MODELS.len(), 39);
        assert_eq!(MODEL_DATA_MANIFEST.files.len(), 39);

        let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/providers/data");
        let manifest_providers = MODEL_DATA_MANIFEST
            .files
            .keys()
            .map(|name| name.trim_end_matches(".json"))
            .collect::<BTreeSet<_>>();
        let generated_providers = MODELS.keys().copied().collect::<BTreeSet<_>>();
        assert_eq!(manifest_providers, generated_providers);

        for (filename, expected_hash) in &MODEL_DATA_MANIFEST.files {
            let bytes = std::fs::read(data_dir.join(filename)).expect("catalog shard");
            assert_eq!(format!("{:x}", Sha256::digest(&bytes)), *expected_hash);
            let groups: serde_json::Value =
                serde_json::from_slice(&bytes).expect("API-grouped catalog JSON");
            let provider = filename.trim_end_matches(".json");
            let catalog = MODELS[provider];
            let mut seen = BTreeSet::new();
            for (api, values) in groups.as_object().expect("API groups") {
                for (id, value) in values.as_object().expect("model group") {
                    assert!(seen.insert(id), "duplicate {provider}/{id}");
                    assert_eq!(value["id"], id.as_str());
                    assert_eq!(value["provider"], provider);
                    assert_eq!(value["api"], api.as_str());
                    let parsed = &catalog[id];
                    assert_eq!(parsed.id, *id);
                    assert_eq!(parsed.provider.as_str(), provider);
                    assert_eq!(parsed.api.as_str(), api);
                }
            }
            assert_eq!(seen.len(), catalog.len());
        }
    }

    /// Ports the catalog assertions in pi `test/baseten-models.test.ts:17-75`.
    #[test]
    fn baseten_reasoning_catalog_matches_pi() {
        let glm = model("baseten", "zai-org/GLM-5.2");
        assert_eq!(glm.api, Api::from("openai-completions"));
        assert_eq!(glm.base_url, "https://inference.baseten.co/v1");
        assert!(glm.reasoning);
        assert_eq!(glm.input, [ModelInput::Text, ModelInput::Image]);
        assert_eq!(glm.context_window, 1_048_576.0);
        assert_eq!(glm.max_tokens, 262_144.0);
        assert_eq!(glm.cost.rates.input, 1.4);
        assert_eq!(glm.cost.rates.output, 4.4);

        let kimi = model("baseten", "moonshotai/Kimi-K2.6");
        assert_eq!(
            get_supported_thinking_levels(kimi),
            [ModelThinkingLevel::Off, ModelThinkingLevel::High]
        );
    }

    /// Ports pi `test/together-models.test.ts:16-78` catalog assertions.
    #[test]
    fn together_models_preserve_catalog_metadata() {
        let kimi = model("together", "moonshotai/Kimi-K2.6");
        assert_eq!(kimi.api.as_str(), "openai-completions");
        assert_eq!(kimi.provider.as_str(), "together");
        assert_eq!(kimi.base_url, "https://api.together.ai/v1");
        assert_eq!(kimi.context_window, 262_144.0);
        assert_eq!(kimi.max_tokens, 131_000.0);
        assert_eq!(kimi.cost.rates.input, 1.2);
        assert_eq!(kimi.cost.rates.output, 4.5);
        assert_eq!(
            get_supported_thinking_levels(model("together", "openai/gpt-oss-120b")),
            [
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
            ]
        );
    }

    /// Ports pi `test/qwen-token-plan-models.test.ts:113-139,175-190`.
    #[test]
    fn qwen_token_plan_allowlists_and_reasoning_maps_match() {
        let expected_individual = BTreeSet::from([
            "deepseek-v4-flash-0731",
            "deepseek-v4-pro",
            "deepseek-v4-pro-0813",
            "glm-5.2",
            "qwen3.6-flash",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.8-max",
        ]);
        assert_eq!(
            MODELS["qwen-token-plan-individual"]
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            expected_individual
        );
        for provider in ["qwen-token-plan", "qwen-token-plan-cn"] {
            assert!(MODELS[provider].contains_key("kimi-k2.7-code"));
            assert!(!MODELS[provider].contains_key("qwen-image-2.0"));
            assert_eq!(
                get_supported_thinking_levels(model(provider, "glm-5.2")),
                [
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Max,
                ]
            );
        }
    }

    /// Ports pi `test/zai-coding-plan-models.test.ts:4-52`.
    #[test]
    fn zai_coding_plan_models_and_reference_costs_match() {
        let vision = model("zai-coding-cn", "glm-4.6v");
        assert_eq!(vision.api.as_str(), "openai-completions");
        assert_eq!(
            vision.base_url,
            "https://open.bigmodel.cn/api/coding/paas/v4"
        );
        assert_eq!(vision.input, [ModelInput::Text, ModelInput::Image]);
        assert_eq!(vision.cost.rates.input, 0.3);
        assert_eq!(vision.cost.rates.output, 0.9);
        assert_eq!(vision.max_tokens, 32_768.0);
        assert_eq!(model("zai", "glm-5.2").cost.rates.input, 1.4);
        assert_eq!(model("zai-coding-cn", "glm-5.1").cost.rates.output, 4.4);
        for provider in ["zai", "zai-coding-cn"] {
            assert_eq!(model(provider, "glm-5.2-highspeed").cost.rates.input, 0.0);
            assert_eq!(model(provider, "glm-5.3").cost.rates.output, 0.0);
        }
    }

    /// Ports pi `test/supports-xhigh.test.ts` over the generated catalogs.
    #[test]
    fn supported_thinking_levels_match_generated_maps() {
        let cases = [
            (
                "openai",
                "gpt-5.6-sol",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "openai",
                "gpt-5.5-pro",
                vec![
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                ],
            ),
            (
                "deepseek",
                "deepseek-v4-flash",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "openrouter",
                "deepseek/deepseek-v4-flash",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                ],
            ),
            (
                "xai",
                "grok-4.6",
                vec![
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                ],
            ),
        ];
        for (provider, id, expected) in cases {
            assert_eq!(get_supported_thinking_levels(model(provider, id)), expected);
        }
    }

    /// Ports pi `test/xiaomi-models.test.ts`,
    /// `test/openrouter-cache-control-models.test.ts`, and
    /// `test/xai-responses.test.ts:115-134`.
    #[test]
    fn provider_catalog_exclusions_routing_and_cache_metadata_match_pi() {
        for provider in [
            "xiaomi",
            "xiaomi-token-plan-cn",
            "xiaomi-token-plan-ams",
            "xiaomi-token-plan-sgp",
        ] {
            for removed in ["mimo-v2-flash", "mimo-v2-omni", "mimo-v2-pro"] {
                assert!(!MODELS[provider].contains_key(removed));
            }
            for replacement in ["mimo-v2.5", "mimo-v2.5-pro"] {
                assert!(MODELS[provider].contains_key(replacement));
            }
        }

        for id in [
            "~anthropic/claude-fable-latest",
            "~anthropic/claude-haiku-latest",
            "~anthropic/claude-opus-latest",
            "~anthropic/claude-sonnet-latest",
        ] {
            let compat = compat_as::<crate::types::OpenAICompletionsCompat>("openrouter", id);
            assert_eq!(
                compat.cache_control_format,
                Some(CacheControlFormat::Anthropic)
            );
        }

        for removed in [
            "grok-3",
            "grok-3-fast",
            "grok-4.20-0309-non-reasoning",
            "grok-4.20-0309-reasoning",
            "grok-code-fast-1",
        ] {
            assert!(!MODELS["xai"].contains_key(removed));
        }
        assert!(
            MODELS["xai"]
                .values()
                .all(|model| model.api == Api::from("openai-responses"))
        );
        assert_eq!(
            get_supported_thinking_levels(model("xai", "grok-4.3")),
            [
                ModelThinkingLevel::Off,
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
            ]
        );
    }

    /// Ports the catalog-only assertions from pi `test/providers.test.ts:58-91`,
    /// `test/bedrock-models.test.ts:27-36`, and `test/fireworks-models.test.ts:21-58,86-164`.
    #[test]
    fn cross_provider_catalog_metadata_and_reference_pricing_match_pi() {
        let gpt4o = compat_as::<crate::types::OpenAIResponsesCompat>("openai", "gpt-4o");
        assert_eq!(gpt4o.supports_strict_mode, Some(true));
        assert_eq!(gpt4o.supports_open_ai_grammar_tools, None);
        let gpt54 = compat_as::<crate::types::OpenAIResponsesCompat>("openai", "gpt-5.4");
        assert_eq!(gpt54.supports_strict_mode, Some(true));
        assert_eq!(gpt54.supports_open_ai_grammar_tools, Some(true));

        for provider in ["moonshotai", "moonshotai-cn"] {
            let kimi = model(provider, "kimi-k3");
            assert_eq!(kimi.cost.rates.input, 3.0);
            assert_eq!(kimi.cost.rates.output, 15.0);
            assert_eq!(kimi.cost.rates.cache_read, 0.3);
            assert_eq!(kimi.cost.rates.cache_write, 0.0);
        }
        assert_eq!(model("kimi-coding", "k3").cost.rates.input, 3.0);
        assert_eq!(
            model("kimi-coding", "kimi-for-coding-highspeed")
                .cost
                .rates
                .output,
            8.0
        );

        assert!(!MODELS["amazon-bedrock"].is_empty());
        assert!(MODELS["amazon-bedrock"].contains_key("global.anthropic.claude-opus-5"));
        assert!(!MODELS["amazon-bedrock"].contains_key("anthropic.claude-opus-5"));

        let kimi = model("fireworks", "accounts/fireworks/models/kimi-k2p6");
        assert_eq!(kimi.api.as_str(), "anthropic-messages");
        assert_eq!(kimi.base_url, "https://api.fireworks.ai/inference");
        assert_eq!(kimi.input, [ModelInput::Text, ModelInput::Image]);
        assert_eq!(
            (kimi.context_window, kimi.max_tokens),
            (262_000.0, 262_000.0)
        );
        assert_eq!((kimi.cost.rates.input, kimi.cost.rates.output), (0.95, 4.0));
        assert!(MODELS["fireworks"].values().any(|model| {
            model.id.starts_with("accounts/fireworks/routers/")
                && model.id.ends_with("-turbo")
                && model.api.as_str() == "anthropic-messages"
        }));
        let glm = model("fireworks", "accounts/fireworks/models/glm-5p2");
        let glm_fast = model("fireworks", "accounts/fireworks/routers/glm-5p2-fast");
        assert_eq!(glm_fast.api, glm.api);
        assert_eq!(glm_fast.base_url, glm.base_url);
        assert_eq!(glm_fast.compat, glm.compat);
        assert_eq!(glm_fast.thinking_level_map, glm.thinking_level_map);
        let kimi3 = model("fireworks", "accounts/fireworks/models/kimi-k3");
        assert_eq!(kimi3.api.as_str(), "openai-completions");
        assert_eq!(kimi3.base_url, "https://api.fireworks.ai/inference/v1");
        assert_eq!(
            get_supported_thinking_levels(kimi3),
            [
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Max,
            ]
        );
    }

    /// Ports pi `test/anthropic-adaptive-thinking-models.test.ts` and all
    /// catalog cases from `test/supports-xhigh.test.ts`.
    #[test]
    fn adaptive_thinking_and_extended_level_catalog_matrix_match_pi() {
        let expected_adaptive = [
            ("anthropic", "claude-fable-5"),
            ("anthropic", "claude-opus-4-8"),
            ("anthropic", "claude-opus-5"),
            ("anthropic", "claude-sonnet-5"),
            ("cloudflare-ai-gateway", "claude-fable-5"),
            ("kimi-coding", "kimi-for-coding"),
            ("kimi-coding", "k3"),
            ("kimi-coding", "kimi-for-coding-highspeed"),
            ("opencode", "claude-opus-4-8"),
            ("opencode", "claude-opus-5"),
            ("vercel-ai-gateway", "anthropic/claude-opus-4.8"),
            ("vercel-ai-gateway", "anthropic/claude-opus-5"),
            ("vercel-ai-gateway", "anthropic/claude-sonnet-5"),
        ];
        for (provider, id) in expected_adaptive {
            let compat = compat_as::<crate::types::AnthropicMessagesCompat>(provider, id);
            assert_eq!(compat.force_adaptive_thinking, Some(true));
        }

        let cases = [
            (
                "anthropic",
                "claude-opus-4-6",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "anthropic",
                "claude-opus-4-8",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "anthropic",
                "claude-opus-5",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "anthropic",
                "claude-sonnet-4-6",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "anthropic",
                "claude-sonnet-5",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "anthropic",
                "claude-fable-5",
                vec![
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "anthropic",
                "claude-sonnet-4-5",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                ],
            ),
            (
                "opencode-go",
                "deepseek-v4-flash",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "opencode-go",
                "kimi-k2.6",
                vec![ModelThinkingLevel::Off, ModelThinkingLevel::High],
            ),
            (
                "moonshotai",
                "kimi-k2.7-code",
                vec![
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                ],
            ),
            (
                "moonshotai-cn",
                "kimi-k2.7-code",
                vec![
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                ],
            ),
            (
                "moonshotai",
                "kimi-k3",
                vec![
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "moonshotai-cn",
                "kimi-k3",
                vec![
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "kimi-coding",
                "k3",
                vec![
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Max,
                ],
            ),
            ("opencode", "grok-build-0.1", vec![ModelThinkingLevel::High]),
            (
                "openrouter",
                "anthropic/claude-opus-4.6",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "amazon-bedrock",
                "global.anthropic.claude-opus-5",
                vec![
                    ModelThinkingLevel::Off,
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                    ModelThinkingLevel::Max,
                ],
            ),
            (
                "amazon-bedrock",
                "global.anthropic.claude-fable-5",
                vec![
                    ModelThinkingLevel::Minimal,
                    ModelThinkingLevel::Low,
                    ModelThinkingLevel::Medium,
                    ModelThinkingLevel::High,
                    ModelThinkingLevel::Xhigh,
                    ModelThinkingLevel::Max,
                ],
            ),
        ];
        for (provider, id, expected) in cases {
            assert_eq!(
                get_supported_thinking_levels(model(provider, id)),
                expected,
                "{provider}/{id}"
            );
        }
    }
}
