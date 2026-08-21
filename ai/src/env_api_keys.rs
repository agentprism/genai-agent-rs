//! Ambient provider credential discovery ⇐ pi `src/env-api-keys.ts`.

use crate::types::ProviderEnv;
use crate::utils::provider_env::get_provider_env_value;
use std::path::PathBuf;
use std::sync::Mutex;

pub const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";
pub const ANTHROPIC_OAUTH_TOKEN_ENV: &str = "ANTHROPIC_OAUTH_TOKEN";
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

static CACHED_VERTEX_ADC: Mutex<Option<bool>> = Mutex::new(None);

#[allow(deprecated)]
fn home_dir() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

fn api_key_env_vars(provider: &str) -> Option<&'static [&'static str]> {
    Some(match provider {
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"],
        "anthropic" => &[
            ANTHROPIC_AUTH_TOKEN_ENV,
            ANTHROPIC_OAUTH_TOKEN_ENV,
            ANTHROPIC_API_KEY_ENV,
        ],
        "ant-ling" => &["ANT_LING_API_KEY"],
        "qwen-token-plan" | "qwen-token-plan-individual" => &["QWEN_TOKEN_PLAN_API_KEY"],
        "qwen-token-plan-cn" => &["QWEN_TOKEN_PLAN_CN_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "azure-openai-responses" => &["AZURE_OPENAI_API_KEY"],
        "nvidia" => &["NVIDIA_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "google" => &["GEMINI_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "radius" => &["RADIUS_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        "zai-coding-cn" => &["ZAI_CODING_CN_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "baseten" => &["BASETEN_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => &["CLOUDFLARE_API_KEY"],
        "xiaomi" => &["XIAOMI_API_KEY"],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        _ => return None,
    })
}

pub fn find_env_keys(provider: &str, env: Option<&ProviderEnv>) -> Option<Vec<String>> {
    let found = api_key_env_vars(provider)?
        .iter()
        .filter(|name| get_provider_env_value(name, env).is_some())
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    (!found.is_empty()).then_some(found)
}

fn has_vertex_adc_credentials(env: Option<&ProviderEnv>) -> bool {
    if let Some(path) = env
        .and_then(|env| env.get("GOOGLE_APPLICATION_CREDENTIALS"))
        .filter(|value| !value.is_empty())
    {
        return std::path::Path::new(path).exists();
    }
    let mut cached = CACHED_VERTEX_ADC.lock().expect("ADC cache mutex poisoned");
    if let Some(value) = *cached {
        return value;
    }
    let path = get_provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env).map_or_else(
        || {
            let mut path = home_dir();
            path.extend([".config", "gcloud", "application_default_credentials.json"]);
            path
        },
        PathBuf::from,
    );
    let exists = path.exists();
    *cached = Some(exists);
    exists
}

pub fn get_env_api_key(provider: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if let Some(keys) = find_env_keys(provider, env) {
        let key = if provider == "anthropic" {
            keys.iter()
                .find(|key| key.as_str() != ANTHROPIC_AUTH_TOKEN_ENV)
        } else {
            keys.first()
        };
        if let Some(key) = key {
            return get_provider_env_value(key, env);
        }
    }
    if provider == "google-vertex"
        && has_vertex_adc_credentials(env)
        && (get_provider_env_value("GOOGLE_CLOUD_PROJECT", env).is_some()
            || get_provider_env_value("GCLOUD_PROJECT", env).is_some())
        && get_provider_env_value("GOOGLE_CLOUD_LOCATION", env).is_some()
    {
        return Some("<authenticated>".to_owned());
    }
    if provider == "amazon-bedrock"
        && (get_provider_env_value("AWS_PROFILE", env).is_some()
            || (get_provider_env_value("AWS_ACCESS_KEY_ID", env).is_some()
                && get_provider_env_value("AWS_SECRET_ACCESS_KEY", env).is_some())
            || get_provider_env_value("AWS_BEARER_TOKEN_BEDROCK", env).is_some()
            || get_provider_env_value("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", env).is_some()
            || get_provider_env_value("AWS_CONTAINER_CREDENTIALS_FULL_URI", env).is_some()
            || get_provider_env_value("AWS_WEB_IDENTITY_TOKEN_FILE", env).is_some())
    {
        return Some("<authenticated>".to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ports pi `test/env-api-keys.test.ts:56-117` without mutating process env.
    #[test]
    fn ports_provider_and_anthropic_environment_key_rules() {
        let github = ProviderEnv::from([
            ("GITHUB_TOKEN".to_owned(), "generic".to_owned()),
            ("GH_TOKEN".to_owned(), "gh".to_owned()),
        ]);
        assert_eq!(find_env_keys("github-copilot", Some(&github)), None);
        assert_eq!(get_env_api_key("github-copilot", Some(&github)), None);

        let copilot =
            ProviderEnv::from([("COPILOT_GITHUB_TOKEN".to_owned(), "copilot".to_owned())]);
        assert_eq!(
            find_env_keys("github-copilot", Some(&copilot)),
            Some(vec!["COPILOT_GITHUB_TOKEN".to_owned()])
        );
        assert_eq!(
            get_env_api_key("github-copilot", Some(&copilot)).as_deref(),
            Some("copilot")
        );

        let zai = ProviderEnv::from([("ZAI_CODING_CN_API_KEY".to_owned(), "zai-cn".to_owned())]);
        assert_eq!(
            get_env_api_key("zai-coding-cn", Some(&zai)).as_deref(),
            Some("zai-cn")
        );

        let anthropic = ProviderEnv::from([
            (ANTHROPIC_AUTH_TOKEN_ENV.to_owned(), "auth".to_owned()),
            (ANTHROPIC_OAUTH_TOKEN_ENV.to_owned(), "oauth".to_owned()),
            (ANTHROPIC_API_KEY_ENV.to_owned(), "key".to_owned()),
        ]);
        assert_eq!(
            find_env_keys("anthropic", Some(&anthropic)),
            Some(vec![
                ANTHROPIC_AUTH_TOKEN_ENV.to_owned(),
                ANTHROPIC_OAUTH_TOKEN_ENV.to_owned(),
                ANTHROPIC_API_KEY_ENV.to_owned(),
            ])
        );
        assert_eq!(
            get_env_api_key("anthropic", Some(&anthropic)).as_deref(),
            Some("oauth")
        );

        let auth_only =
            ProviderEnv::from([(ANTHROPIC_AUTH_TOKEN_ENV.to_owned(), "auth".to_owned())]);
        assert_eq!(get_env_api_key("anthropic", Some(&auth_only)), None);
        let api_only = ProviderEnv::from([(ANTHROPIC_API_KEY_ENV.to_owned(), "key".to_owned())]);
        assert_eq!(
            get_env_api_key("anthropic", Some(&api_only)).as_deref(),
            Some("key")
        );
    }
}
