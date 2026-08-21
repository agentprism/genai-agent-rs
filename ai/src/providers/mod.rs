pub mod amazon_bedrock;
pub mod ant_ling;
pub mod anthropic;
pub mod baseten;
pub mod cerebras;
pub mod cloudflare_ai_gateway;
pub mod cloudflare_auth;
pub mod cloudflare_stream;
pub mod cloudflare_workers_ai;
pub mod data;
pub mod deepseek;
pub mod faux;
pub mod fireworks;
pub mod github_copilot;
pub mod google;
pub mod google_vertex;
pub mod groq;
pub mod huggingface;
pub mod kimi_coding;
pub mod minimax;
pub mod minimax_cn;
pub mod moonshotai;
pub mod moonshotai_cn;
pub mod nvidia;
pub mod opencode;
pub mod opencode_go;
pub mod qwen_token_plan;
pub mod qwen_token_plan_cn;
pub mod qwen_token_plan_individual;
pub mod together;
pub mod vercel_ai_gateway;
pub mod xiaomi;
pub mod xiaomi_token_plan_ams;
pub mod xiaomi_token_plan_cn;
pub mod xiaomi_token_plan_sgp;
pub mod zai;
pub mod zai_coding_cn;

pub mod amazon_bedrock_models;
pub mod ant_ling_models;
pub mod anthropic_models;
pub mod azure_openai_responses_models;
pub mod baseten_models;
pub mod cerebras_models;
pub mod cloudflare_ai_gateway_models;
pub mod cloudflare_workers_ai_models;
pub mod deepseek_models;
pub mod fireworks_models;
pub mod github_copilot_models;
pub mod google_models;
pub mod google_vertex_models;
pub mod groq_models;
pub mod huggingface_models;
pub mod kimi_coding_models;
pub mod minimax_cn_models;
pub mod minimax_models;
pub mod mistral_models;
pub mod moonshotai_cn_models;
pub mod moonshotai_models;
pub mod nvidia_models;
pub mod openai;
pub mod openai_codex;
pub mod openai_codex_models;
pub mod openai_models;
pub mod opencode_go_models;
pub mod opencode_models;
pub mod openrouter;
pub mod openrouter_models;
pub mod qwen_token_plan_cn_models;
pub mod qwen_token_plan_individual_models;
pub mod qwen_token_plan_models;
pub mod together_models;
pub mod vercel_ai_gateway_models;
pub mod xai;
pub mod xai_models;
pub mod xiaomi_models;
pub mod xiaomi_token_plan_ams_models;
pub mod xiaomi_token_plan_cn_models;
pub mod xiaomi_token_plan_sgp_models;
pub mod zai_coding_cn_models;
pub mod zai_models;

use crate::api::ProviderStreams;
use crate::auth::types::ProviderAuth;
use crate::model_catalog::ModelCatalog;
use crate::models::{CreateProviderOptions, ProviderApi, ProviderRef, create_provider};
use std::sync::Arc;

fn static_provider(
    id: &str,
    name: &str,
    base_url: &str,
    auth: ProviderAuth,
    catalog: &ModelCatalog,
    api: Arc<dyn ProviderStreams>,
) -> ProviderRef {
    create_provider(CreateProviderOptions {
        id: id.to_owned(),
        name: Some(name.to_owned()),
        base_url: Some(base_url.to_owned()),
        headers: None,
        auth,
        models: catalog.values().cloned().collect(),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(api),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ports the factory metadata exercised by pi `test/providers.test.ts` and
    /// each phase-scoped `src/providers/<name>.ts` constructor.
    #[test]
    fn ported_provider_factories_preserve_identity_base_url_and_catalog() {
        let cases: Vec<(ProviderRef, &str)> = vec![
            (ant_ling::ant_ling_provider(), "https://api.ant-ling.com/v1"),
            (amazon_bedrock::amazon_bedrock_provider(), ""),
            (anthropic::anthropic_provider(), "https://api.anthropic.com"),
            (
                baseten::baseten_provider(),
                "https://inference.baseten.co/v1",
            ),
            (cerebras::cerebras_provider(), "https://api.cerebras.ai/v1"),
            (cloudflare_workers_ai::cloudflare_workers_ai_provider(), ""),
            (cloudflare_ai_gateway::cloudflare_ai_gateway_provider(), ""),
            (deepseek::deepseek_provider(), "https://api.deepseek.com"),
            (groq::groq_provider(), "https://api.groq.com/openai/v1"),
            (
                google::google_provider(),
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            (google_vertex::google_vertex_provider(), ""),
            (
                fireworks::fireworks_provider(),
                "https://api.fireworks.ai/inference",
            ),
            (
                github_copilot::github_copilot_provider(),
                "https://api.individual.githubcopilot.com",
            ),
            (
                huggingface::huggingface_provider(),
                "https://router.huggingface.co/v1",
            ),
            (
                moonshotai::moonshotai_provider(),
                "https://api.moonshot.ai/v1",
            ),
            (
                moonshotai_cn::moonshotai_cn_provider(),
                "https://api.moonshot.cn/v1",
            ),
            (
                kimi_coding::kimi_coding_provider(),
                "https://api.kimi.com/coding",
            ),
            (
                minimax::minimax_provider(),
                "https://api.minimax.io/anthropic",
            ),
            (
                minimax_cn::minimax_cn_provider(),
                "https://api.minimaxi.com/anthropic",
            ),
            (
                nvidia::nvidia_provider(),
                "https://integrate.api.nvidia.com/v1",
            ),
            (
                qwen_token_plan::qwen_token_plan_provider(),
                "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
            ),
            (
                qwen_token_plan_cn::qwen_token_plan_cn_provider(),
                "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
            ),
            (
                qwen_token_plan_individual::qwen_token_plan_individual_provider(),
                "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
            ),
            (together::together_provider(), "https://api.together.ai/v1"),
            (xiaomi::xiaomi_provider(), "https://api.xiaomimimo.com/v1"),
            (
                xiaomi_token_plan_ams::xiaomi_token_plan_ams_provider(),
                "https://token-plan-ams.xiaomimimo.com/v1",
            ),
            (
                xiaomi_token_plan_cn::xiaomi_token_plan_cn_provider(),
                "https://token-plan-cn.xiaomimimo.com/v1",
            ),
            (
                xiaomi_token_plan_sgp::xiaomi_token_plan_sgp_provider(),
                "https://token-plan-sgp.xiaomimimo.com/v1",
            ),
            (zai::zai_provider(), "https://api.z.ai/api/coding/paas/v4"),
            (
                zai_coding_cn::zai_coding_cn_provider(),
                "https://open.bigmodel.cn/api/coding/paas/v4",
            ),
            (openai::openai_provider(), "https://api.openai.com/v1"),
            (
                openai_codex::openai_codex_provider(),
                "https://chatgpt.com/backend-api",
            ),
            (
                openrouter::openrouter_provider(),
                "https://openrouter.ai/api/v1",
            ),
            (opencode::opencode_provider(), ""),
            (opencode_go::opencode_go_provider(), ""),
            (
                vercel_ai_gateway::vercel_ai_gateway_provider(),
                "https://ai-gateway.vercel.sh",
            ),
            (xai::xai_provider(), "https://api.x.ai/v1"),
        ];
        for (provider, base_url) in cases {
            let expected_name = match provider.id() {
                "ant-ling" => "Ant Ling",
                "amazon-bedrock" => "Amazon Bedrock",
                "anthropic" => "Anthropic",
                "baseten" => "Baseten",
                "cerebras" => "Cerebras",
                "cloudflare-workers-ai" => "Cloudflare Workers AI",
                "cloudflare-ai-gateway" => "Cloudflare AI Gateway",
                "deepseek" => "DeepSeek",
                "groq" => "Groq",
                "google" => "Google",
                "google-vertex" => "Google Vertex AI",
                "fireworks" => "Fireworks",
                "github-copilot" => "GitHub Copilot",
                "huggingface" => "Hugging Face",
                "moonshotai" => "Moonshot AI",
                "moonshotai-cn" => "Moonshot AI CN",
                "kimi-coding" => "Kimi For Coding",
                "minimax" => "MiniMax",
                "minimax-cn" => "MiniMax CN",
                "nvidia" => "NVIDIA",
                "qwen-token-plan" => "Qwen Token Plan",
                "qwen-token-plan-cn" => "Qwen Token Plan CN",
                "qwen-token-plan-individual" => "Qwen Token Plan Individual",
                "together" => "Together",
                "xiaomi" => "Xiaomi",
                "xiaomi-token-plan-ams" => "Xiaomi Token Plan AMS",
                "xiaomi-token-plan-cn" => "Xiaomi Token Plan CN",
                "xiaomi-token-plan-sgp" => "Xiaomi Token Plan SGP",
                "zai" => "Z.AI",
                "zai-coding-cn" => "Z.AI Coding CN",
                "openai" => "OpenAI",
                "openai-codex" => "OpenAI Codex",
                "openrouter" => "OpenRouter",
                "opencode" => "OpenCode Zen",
                "opencode-go" => "OpenCode Go",
                "vercel-ai-gateway" => "Vercel AI Gateway",
                "xai" => "xAI",
                id => panic!("unexpected provider {id}"),
            };
            assert_eq!(provider.name(), expected_name);
            if base_url.is_empty() {
                assert_eq!(provider.base_url(), None);
            } else {
                assert_eq!(provider.base_url(), Some(base_url));
            }
            let models = provider.get_models().expect("static catalog");
            assert!(!models.is_empty(), "{} has no models", provider.id());
            assert!(
                models
                    .iter()
                    .all(|model| model.provider.as_str() == provider.id())
            );
            assert!(provider.auth().api_key.is_some() || provider.auth().oauth.is_some());
        }
    }

    /// Ports pi `test/qwen-token-plan-models.test.ts:121-125` and provider env auth declarations.
    #[test]
    fn factory_auth_methods_match_provider_capabilities() {
        let individual = qwen_token_plan_individual::qwen_token_plan_individual_provider();
        assert!(individual.auth().api_key.is_some());
        let codex = openai_codex::openai_codex_provider();
        assert!(codex.auth().api_key.is_none());
        assert!(codex.auth().oauth.is_some());
        for provider in [openrouter::openrouter_provider(), xai::xai_provider()] {
            assert!(provider.auth().api_key.is_some());
            assert!(provider.auth().oauth.is_some());
        }
        for provider in [
            anthropic::anthropic_provider(),
            kimi_coding::kimi_coding_provider(),
            github_copilot::github_copilot_provider(),
        ] {
            assert!(provider.auth().api_key.is_some());
            assert!(provider.auth().oauth.is_some());
        }
    }
}
