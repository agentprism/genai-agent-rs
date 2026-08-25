//! Aggregation of all provider leaf crates in pinned Pi order.

#![deny(missing_docs)]

use pi_ai::{
    HttpTransport, LocalHttpTransport, LocalProviderRegistration, ModelDescriptor,
    ProviderRegistration,
};
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

pub use pi_ai_provider_common::{LocalProviderInputs, ProviderInputs};

/// Static remaining-provider catalog IDs. Radius is dynamic and is composed
/// separately by this all-provider crate.
pub const REMAINING_PROVIDER_IDS: &[&str] = &[
    "ant-ling",
    "azure-openai-responses",
    "baseten",
    "cerebras",
    "cloudflare-ai-gateway",
    "cloudflare-workers-ai",
    "fireworks",
    "github-copilot",
    "groq",
    "huggingface",
    "kimi-coding",
    "minimax",
    "minimax-cn",
    "moonshotai",
    "moonshotai-cn",
    "nvidia",
    "opencode",
    "opencode-go",
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "qwen-token-plan-individual",
    "together",
    "vercel-ai-gateway",
    "xai",
    "xiaomi",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-sgp",
    "zai",
    "zai-coding-cn",
];

/// Error while composing provider-owned leaf registrations or catalogs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllProvidersError(String);

impl fmt::Display for AllProvidersError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AllProvidersError {}

fn all_error(error: impl fmt::Display) -> AllProvidersError {
    AllProvidersError(error.to_string())
}

/// Loads one static catalog by explicitly importing its owning leaf crate.
pub fn remaining_provider_models(id: &str) -> Result<Vec<ModelDescriptor>, AllProvidersError> {
    match id {
        "ant-ling" => pi_ai_ant_ling::models().map_err(all_error),
        "azure-openai-responses" => pi_ai_azure_openai_responses::models().map_err(all_error),
        "baseten" => pi_ai_baseten::models().map_err(all_error),
        "cerebras" => pi_ai_cerebras::models().map_err(all_error),
        "cloudflare-ai-gateway" => pi_ai_cloudflare_ai_gateway::models().map_err(all_error),
        "cloudflare-workers-ai" => pi_ai_cloudflare_workers_ai::models().map_err(all_error),
        "fireworks" => pi_ai_fireworks::models().map_err(all_error),
        "github-copilot" => pi_ai_github_copilot::models().map_err(all_error),
        "groq" => pi_ai_groq::models().map_err(all_error),
        "huggingface" => pi_ai_huggingface::models().map_err(all_error),
        "kimi-coding" => pi_ai_kimi_coding::models().map_err(all_error),
        "minimax" => pi_ai_minimax::models().map_err(all_error),
        "minimax-cn" => pi_ai_minimax_cn::models().map_err(all_error),
        "moonshotai" => pi_ai_moonshotai::models().map_err(all_error),
        "moonshotai-cn" => pi_ai_moonshotai_cn::models().map_err(all_error),
        "nvidia" => pi_ai_nvidia::models().map_err(all_error),
        "opencode" => pi_ai_opencode::models().map_err(all_error),
        "opencode-go" => pi_ai_opencode_go::models().map_err(all_error),
        "qwen-token-plan" => pi_ai_qwen_token_plan::models().map_err(all_error),
        "qwen-token-plan-cn" => pi_ai_qwen_token_plan_cn::models().map_err(all_error),
        "qwen-token-plan-individual" => {
            pi_ai_qwen_token_plan_individual::models().map_err(all_error)
        }
        "together" => pi_ai_together::models().map_err(all_error),
        "vercel-ai-gateway" => pi_ai_vercel_ai_gateway::models().map_err(all_error),
        "xai" => pi_ai_xai::models().map_err(all_error),
        "xiaomi" => pi_ai_xiaomi::models().map_err(all_error),
        "xiaomi-token-plan-ams" => pi_ai_xiaomi_token_plan_ams::models().map_err(all_error),
        "xiaomi-token-plan-cn" => pi_ai_xiaomi_token_plan_cn::models().map_err(all_error),
        "xiaomi-token-plan-sgp" => pi_ai_xiaomi_token_plan_sgp::models().map_err(all_error),
        "zai" => pi_ai_zai::models().map_err(all_error),
        "zai-coding-cn" => pi_ai_zai_coding_cn::models().map_err(all_error),
        other => Err(AllProvidersError(format!(
            "unknown remaining provider catalog: {other}"
        ))),
    }
}

/// Dependencies required to construct the complete Send provider set.
#[derive(Clone)]
pub struct BuiltinProviderInputs {
    /// Raw HTTP transport shared by HTTP API families and provider OAuth flows.
    pub http: Arc<dyn HttpTransport>,
    /// Host-owned AWS signer/client boundary required by Amazon Bedrock.
    pub bedrock: Arc<dyn pi_ai_bedrock::BedrockSigner>,
    /// Host-injected construction-time provider environment.
    pub environment: BTreeMap<String, String>,
}

/// Dependencies required to construct the complete local provider set.
#[derive(Clone)]
pub struct LocalBuiltinProviderInputs {
    /// Local raw HTTP transport shared by HTTP API families and OAuth flows.
    pub http: Rc<dyn LocalHttpTransport>,
    /// Local host-owned AWS signer/client boundary required by Amazon Bedrock.
    pub bedrock: Rc<dyn pi_ai_bedrock::LocalBedrockSigner>,
    /// Local host-injected construction-time provider environment.
    pub environment: BTreeMap<String, String>,
}

/// Failure while constructing the complete built-in provider set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinProvidersError {
    provider: &'static str,
    detail: String,
}

impl BuiltinProvidersError {
    fn new(provider: &'static str, error: impl fmt::Display) -> Self {
        Self {
            provider,
            detail: error.to_string(),
        }
    }
}

impl fmt::Display for BuiltinProvidersError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to construct built-in provider {}: {}",
            self.provider, self.detail
        )
    }
}

impl std::error::Error for BuiltinProvidersError {}

/// Builds every remaining Send provider in pinned `providers/all.ts` order.
pub fn remaining_providers(
    inputs: ProviderInputs,
) -> Result<Vec<pi_ai::ProviderRegistration>, AllProvidersError> {
    Ok(vec![
        pi_ai_ant_ling::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_azure_openai_responses::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_baseten::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_cerebras::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_cloudflare_ai_gateway::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_cloudflare_workers_ai::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_fireworks::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_github_copilot::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_groq::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_huggingface::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_kimi_coding::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_minimax::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_minimax_cn::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_moonshotai::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_moonshotai_cn::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_nvidia::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_opencode::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_opencode_go::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_qwen_token_plan::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_qwen_token_plan_cn::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_qwen_token_plan_individual::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_radius::radius_provider(inputs.http.clone()).map_err(all_error)?,
        pi_ai_together::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_vercel_ai_gateway::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xai::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xiaomi::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xiaomi_token_plan_ams::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xiaomi_token_plan_cn::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xiaomi_token_plan_sgp::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_zai::provider(inputs.clone()).map_err(all_error)?,
        pi_ai_zai_coding_cn::provider(inputs.clone()).map_err(all_error)?,
    ])
}

/// Builds every remaining local provider in pinned order.
pub fn local_remaining_providers(
    inputs: LocalProviderInputs,
) -> Result<Vec<pi_ai::LocalProviderRegistration>, AllProvidersError> {
    Ok(vec![
        pi_ai_ant_ling::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_azure_openai_responses::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_baseten::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_cerebras::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_cloudflare_ai_gateway::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_cloudflare_workers_ai::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_fireworks::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_github_copilot::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_groq::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_huggingface::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_kimi_coding::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_minimax::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_minimax_cn::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_moonshotai::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_moonshotai_cn::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_nvidia::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_opencode::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_opencode_go::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_qwen_token_plan::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_qwen_token_plan_cn::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_qwen_token_plan_individual::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_radius::local_radius_provider(inputs.http.clone()).map_err(all_error)?,
        pi_ai_together::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_vercel_ai_gateway::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xai::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xiaomi::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xiaomi_token_plan_ams::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xiaomi_token_plan_cn::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_xiaomi_token_plan_sgp::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_zai::local_provider(inputs.clone()).map_err(all_error)?,
        pi_ai_zai_coding_cn::local_provider(inputs.clone()).map_err(all_error)?,
    ])
}

fn take_send(
    providers: &mut Vec<ProviderRegistration>,
    id: &'static str,
) -> Result<ProviderRegistration, BuiltinProvidersError> {
    let Some(index) = providers
        .iter()
        .position(|provider| provider.descriptor.id.as_str() == id)
    else {
        return Err(BuiltinProvidersError {
            provider: id,
            detail: "remaining provider aggregation omitted registration".into(),
        });
    };
    Ok(providers.remove(index))
}

fn take_local(
    providers: &mut Vec<LocalProviderRegistration>,
    id: &'static str,
) -> Result<LocalProviderRegistration, BuiltinProvidersError> {
    let Some(index) = providers
        .iter()
        .position(|provider| provider.descriptor.id.as_str() == id)
    else {
        return Err(BuiltinProvidersError {
            provider: id,
            detail: "remaining provider aggregation omitted local registration".into(),
        });
    };
    Ok(providers.remove(index))
}

/// Builds all chat providers in the exact order of pinned Pi's
/// `providers/all.ts::builtinProviders`.
pub fn builtin_providers(
    inputs: BuiltinProviderInputs,
) -> Result<Vec<ProviderRegistration>, BuiltinProvidersError> {
    let mut remaining = remaining_providers(ProviderInputs {
        http: Arc::clone(&inputs.http),
        environment: inputs.environment.clone(),
    })
    .map_err(|error| BuiltinProvidersError::new("remaining", error))?;
    let mut providers = Vec::with_capacity(40);
    providers.push(
        pi_ai_bedrock::bedrock_provider(inputs.bedrock)
            .map_err(|error| BuiltinProvidersError::new("amazon-bedrock", error))?,
    );
    providers.push(take_send(&mut remaining, "ant-ling")?);
    providers.push(
        pi_ai_anthropic::anthropic_provider(Arc::clone(&inputs.http))
            .map_err(|error| BuiltinProvidersError::new("anthropic", error))?,
    );
    for id in [
        "azure-openai-responses",
        "baseten",
        "cerebras",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
    ] {
        providers.push(take_send(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_deepseek::provider(ProviderInputs {
            http: Arc::clone(&inputs.http),
            environment: inputs.environment.clone(),
        })
        .map_err(|error| BuiltinProvidersError::new("deepseek", error))?,
    );
    for id in ["fireworks", "github-copilot"] {
        providers.push(take_send(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_google::google_provider(Arc::clone(&inputs.http))
            .map_err(|error| BuiltinProvidersError::new("google", error))?,
    );
    providers.push(
        pi_ai_google_vertex::provider(ProviderInputs {
            http: Arc::clone(&inputs.http),
            environment: inputs.environment.clone(),
        })
        .map_err(|error| BuiltinProvidersError::new("google-vertex", error))?,
    );
    for id in [
        "groq",
        "huggingface",
        "kimi-coding",
        "minimax",
        "minimax-cn",
    ] {
        providers.push(take_send(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_mistral::mistral_provider(Arc::clone(&inputs.http))
            .map_err(|error| BuiltinProvidersError::new("mistral", error))?,
    );
    for id in ["moonshotai", "moonshotai-cn", "nvidia"] {
        providers.push(take_send(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_openai::openai_provider(Arc::clone(&inputs.http))
            .map_err(|error| BuiltinProvidersError::new("openai", error))?,
    );
    providers.push(
        pi_ai_openai_codex::provider(ProviderInputs {
            http: Arc::clone(&inputs.http),
            environment: inputs.environment.clone(),
        })
        .map_err(|error| BuiltinProvidersError::new("openai-codex", error))?,
    );
    for id in ["opencode", "opencode-go"] {
        providers.push(take_send(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_openrouter::provider(ProviderInputs {
            http: Arc::clone(&inputs.http),
            environment: inputs.environment.clone(),
        })
        .map_err(|error| BuiltinProvidersError::new("openrouter", error))?,
    );
    for id in [
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
        "radius",
        "together",
        "vercel-ai-gateway",
        "xai",
        "xiaomi",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-sgp",
        "zai",
        "zai-coding-cn",
    ] {
        providers.push(take_send(&mut remaining, id)?);
    }
    debug_assert!(remaining.is_empty());
    Ok(providers)
}

/// Builds all local chat providers in pinned Pi `builtinProviders` order.
pub fn local_builtin_providers(
    inputs: LocalBuiltinProviderInputs,
) -> Result<Vec<LocalProviderRegistration>, BuiltinProvidersError> {
    let mut remaining = local_remaining_providers(LocalProviderInputs {
        http: Rc::clone(&inputs.http),
        environment: inputs.environment.clone(),
    })
    .map_err(|error| BuiltinProvidersError::new("remaining", error))?;
    let mut providers = Vec::with_capacity(40);
    providers.push(
        pi_ai_bedrock::local_bedrock_provider(inputs.bedrock)
            .map_err(|error| BuiltinProvidersError::new("amazon-bedrock", error))?,
    );
    providers.push(take_local(&mut remaining, "ant-ling")?);
    providers.push(
        pi_ai_anthropic::local_anthropic_provider(Rc::clone(&inputs.http))
            .map_err(|error| BuiltinProvidersError::new("anthropic", error))?,
    );
    for id in [
        "azure-openai-responses",
        "baseten",
        "cerebras",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
    ] {
        providers.push(take_local(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_deepseek::local_provider(LocalProviderInputs {
            http: Rc::clone(&inputs.http),
            environment: inputs.environment.clone(),
        })
        .map_err(|error| BuiltinProvidersError::new("deepseek", error))?,
    );
    for id in ["fireworks", "github-copilot"] {
        providers.push(take_local(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_google::local_google_provider(Rc::clone(&inputs.http))
            .map_err(|error| BuiltinProvidersError::new("google", error))?,
    );
    providers.push(
        pi_ai_google_vertex::local_provider(LocalProviderInputs {
            http: Rc::clone(&inputs.http),
            environment: inputs.environment.clone(),
        })
        .map_err(|error| BuiltinProvidersError::new("google-vertex", error))?,
    );
    for id in [
        "groq",
        "huggingface",
        "kimi-coding",
        "minimax",
        "minimax-cn",
    ] {
        providers.push(take_local(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_mistral::local_mistral_provider(Rc::clone(&inputs.http))
            .map_err(|error| BuiltinProvidersError::new("mistral", error))?,
    );
    for id in ["moonshotai", "moonshotai-cn", "nvidia"] {
        providers.push(take_local(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_openai::local_openai_provider(Rc::clone(&inputs.http))
            .map_err(|error| BuiltinProvidersError::new("openai", error))?,
    );
    providers.push(
        pi_ai_openai_codex::local_provider(LocalProviderInputs {
            http: Rc::clone(&inputs.http),
            environment: inputs.environment.clone(),
        })
        .map_err(|error| BuiltinProvidersError::new("openai-codex", error))?,
    );
    for id in ["opencode", "opencode-go"] {
        providers.push(take_local(&mut remaining, id)?);
    }
    providers.push(
        pi_ai_openrouter::local_provider(LocalProviderInputs {
            http: Rc::clone(&inputs.http),
            environment: inputs.environment.clone(),
        })
        .map_err(|error| BuiltinProvidersError::new("openrouter", error))?,
    );
    for id in [
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
        "radius",
        "together",
        "vercel-ai-gateway",
        "xai",
        "xiaomi",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-sgp",
        "zai",
        "zai-coding-cn",
    ] {
        providers.push(take_local(&mut remaining, id)?);
    }
    debug_assert!(remaining.is_empty());
    Ok(providers)
}
