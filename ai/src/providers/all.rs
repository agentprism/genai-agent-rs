pub use super::radius::{RadiusProviderOptions, radius_provider};
use super::{
    amazon_bedrock::amazon_bedrock_provider, ant_ling::ant_ling_provider,
    anthropic::anthropic_provider, baseten::baseten_provider, cerebras::cerebras_provider,
    cloudflare_ai_gateway::cloudflare_ai_gateway_provider,
    cloudflare_workers_ai::cloudflare_workers_ai_provider, deepseek::deepseek_provider,
    fireworks::fireworks_provider, github_copilot::github_copilot_provider,
    google::google_provider, google_vertex::google_vertex_provider, groq::groq_provider,
    huggingface::huggingface_provider, kimi_coding::kimi_coding_provider,
    minimax::minimax_provider, minimax_cn::minimax_cn_provider, moonshotai::moonshotai_provider,
    moonshotai_cn::moonshotai_cn_provider, nvidia::nvidia_provider, openai::openai_provider,
    openai_codex::openai_codex_provider, opencode::opencode_provider,
    opencode_go::opencode_go_provider, openrouter::openrouter_provider,
    qwen_token_plan::qwen_token_plan_provider, qwen_token_plan_cn::qwen_token_plan_cn_provider,
    qwen_token_plan_individual::qwen_token_plan_individual_provider, together::together_provider,
    vercel_ai_gateway::vercel_ai_gateway_provider, xai::xai_provider, xiaomi::xiaomi_provider,
    xiaomi_token_plan_ams::xiaomi_token_plan_ams_provider,
    xiaomi_token_plan_cn::xiaomi_token_plan_cn_provider,
    xiaomi_token_plan_sgp::xiaomi_token_plan_sgp_provider, zai::zai_provider,
    zai_coding_cn::zai_coding_cn_provider,
};
use crate::models::{CreateModelsOptions, MutableModels, ProviderRef, create_models};
use crate::models_generated::MODELS;
use crate::providers::data::model_data_generated_at;
use crate::types::Model;

pub type BuiltinProvider = &'static str;

pub fn get_builtin_model(provider: &str, model_id: &str) -> Option<&'static Model> {
    MODELS.get(provider).and_then(|models| models.get(model_id))
}

pub fn get_builtin_providers() -> Vec<BuiltinProvider> {
    MODELS.keys().copied().collect()
}

pub fn get_builtin_model_data_generated_at() -> Option<i64> {
    model_data_generated_at()
}

pub fn get_builtin_models(provider: &str) -> Vec<Model> {
    MODELS
        .get(provider)
        .map_or_else(Vec::new, |models| models.values().cloned().collect())
}

pub fn builtin_providers() -> Vec<ProviderRef> {
    vec![
        amazon_bedrock_provider(),
        ant_ling_provider(),
        anthropic_provider(),
        baseten_provider(),
        cerebras_provider(),
        cloudflare_ai_gateway_provider(),
        cloudflare_workers_ai_provider(),
        deepseek_provider(),
        fireworks_provider(),
        github_copilot_provider(),
        google_provider(),
        google_vertex_provider(),
        groq_provider(),
        huggingface_provider(),
        kimi_coding_provider(),
        minimax_provider(),
        minimax_cn_provider(),
        moonshotai_provider(),
        moonshotai_cn_provider(),
        nvidia_provider(),
        openai_provider(),
        openai_codex_provider(),
        opencode_provider(),
        opencode_go_provider(),
        openrouter_provider(),
        qwen_token_plan_provider(),
        qwen_token_plan_cn_provider(),
        qwen_token_plan_individual_provider(),
        radius_provider(RadiusProviderOptions::default()),
        together_provider(),
        vercel_ai_gateway_provider(),
        xai_provider(),
        xiaomi_provider(),
        xiaomi_token_plan_ams_provider(),
        xiaomi_token_plan_cn_provider(),
        xiaomi_token_plan_sgp_provider(),
        zai_provider(),
        zai_coding_cn_provider(),
    ]
}

pub fn builtin_models(options: Option<CreateModelsOptions>) -> MutableModels {
    let models = create_models(options);
    for provider in builtin_providers() {
        models.set_provider(provider);
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Ports pi `test/providers.test.ts:36-67` and `src/providers/all.ts:61-141`.
    #[test]
    fn builtin_catalog_accessors_and_registered_provider_set_match_pi() {
        let models = builtin_models(None);
        let providers = models.get_providers();
        assert_eq!(providers.len(), builtin_providers().len());
        assert!(
            providers
                .iter()
                .any(|provider| provider.id() == "anthropic")
        );
        assert_eq!(
            models
                .get_model("anthropic", "claude-haiku-4-5")
                .expect("model")
                .api
                .as_str(),
            "anthropic-messages"
        );
        assert!(models.get_models(None).len() > 500);
        for provider in providers {
            let catalog = models.get_models(Some(provider.id()));
            if provider.id() == "radius" {
                assert!(catalog.is_empty());
            } else {
                assert!(!catalog.is_empty(), "{} has no models", provider.id());
            }
            assert!(
                catalog
                    .iter()
                    .all(|model| model.provider.as_str() == provider.id())
            );
        }
        assert!(get_builtin_model("openai", "gpt-4o").is_some());
        assert!(get_builtin_model("openai", "does-not-exist").is_none());
        assert!(get_builtin_models("does-not-exist").is_empty());
        assert!(get_builtin_model_data_generated_at().is_some());

        let generated = get_builtin_providers().into_iter().collect::<BTreeSet<_>>();
        let registered = builtin_providers()
            .into_iter()
            .map(|provider| provider.id().to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(generated.len(), 39);
        assert!(!registered.contains("azure-openai-responses"));
        assert!(!registered.contains("mistral"));
        assert!(registered.contains("radius"));
        assert_eq!(registered.len(), 38);
    }
}
