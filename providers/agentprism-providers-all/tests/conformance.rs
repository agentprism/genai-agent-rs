//! Pinned all-provider aggregation conformance.

use agentprism_ai::*;
use agentprism_providers_all::*;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone)]
struct NoNetwork;

impl HttpTransport for NoNetwork {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async { Err(TransportError::new("no_network", "hermetic test transport")) })
    }
}

impl LocalHttpTransport for NoNetwork {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async { Err(TransportError::new("no_network", "hermetic test transport")) })
    }
}

impl agentprism_bedrock::BedrockSigner for NoNetwork {
    fn execute(
        &self,
        _config: agentprism_bedrock::BedrockSigningConfig,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<
        '_,
        Result<agentprism_bedrock::BedrockSignerResponse, agentprism_bedrock::BedrockSignerError>,
    > {
        unreachable!("provider construction does not execute the signer")
    }
}

impl agentprism_bedrock::LocalBedrockSigner for NoNetwork {
    fn execute(
        &self,
        _config: agentprism_bedrock::BedrockSigningConfig,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<
        '_,
        Result<
            agentprism_bedrock::LocalBedrockSignerResponse,
            agentprism_bedrock::BedrockSignerError,
        >,
    > {
        unreachable!("provider construction does not execute the signer")
    }
}

#[test]
fn pi_ai_providers_all_catalogs_match_pinned_publication() {
    // Pi basis: packages/ai/src/providers/all.ts and every imported generated
    // `providers/data/*.json` catalog at 8fa7eebd2.
    for provider in REMAINING_PROVIDER_IDS {
        let models = remaining_provider_models(provider).unwrap_or_else(|error| {
            panic!("{provider} catalog failed: {error}");
        });
        assert!(!models.is_empty(), "{provider}");
        assert!(
            models
                .iter()
                .all(|model| { model.common.model_ref.provider == ProviderId::new(*provider) })
        );
    }
}

fn pinned_model(provider: &str, model: &str) -> ModelDescriptor {
    remaining_provider_models(provider)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.common.model_ref.model.as_str() == model)
        .unwrap_or_else(|| panic!("missing pinned model {provider}/{model}"))
}

#[test]
fn remaining_provider_catalogs_preserve_pinned_compat_and_headers_pi_exact() {
    // Pi basis: packages/ai/test/baseten-models.test.ts,
    // fireworks-models.test.ts, qwen-token-plan-models.test.ts,
    // together-models.test.ts, xiaomi-models.test.ts,
    // zai-coding-plan-models.test.ts, and providers/github-copilot.ts.
    let baseten = pinned_model("baseten", "zai-org/GLM-5.2");
    let ApiModelConfig::OpenAiCompletions(baseten_config) = baseten.api else {
        panic!("Baseten GLM 5.2 must use OpenAI Completions")
    };
    assert_eq!(
        baseten_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::Baseten)
    );
    assert_eq!(baseten_config.compat.supports_reasoning_effort, Some(true));
    assert_eq!(
        baseten_config.compat.supports_usage_in_streaming,
        Some(true)
    );
    assert_eq!(baseten_config.compat.supports_strict_mode, Some(true));
    assert_eq!(
        baseten_config.compat.supports_long_cache_retention,
        Some(false)
    );
    assert_eq!(
        serde_json::to_value(baseten_config.compat.chat_template_args).unwrap(),
        serde_json::json!({"enable_thinking":{"$var":"thinking.enabled"}})
    );

    let fireworks = pinned_model("fireworks", "accounts/fireworks/models/kimi-k2p6");
    let ApiModelConfig::AnthropicMessages(fireworks_config) = fireworks.api else {
        panic!("Fireworks Kimi K2.6 must use Anthropic Messages")
    };
    assert_eq!(
        fireworks_config.compat.send_session_affinity_headers,
        Some(true)
    );
    assert_eq!(
        fireworks_config.compat.supports_eager_tool_input_streaming,
        Some(false)
    );
    assert_eq!(
        fireworks_config.compat.supports_cache_control_on_tools,
        Some(false)
    );

    let together = pinned_model("together", "moonshotai/Kimi-K2.6");
    let ApiModelConfig::OpenAiCompletions(together_config) = together.api else {
        panic!("Together Kimi K2.6 must use OpenAI Completions")
    };
    assert_eq!(
        together_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::Together)
    );
    assert_eq!(
        together_config.compat.supports_reasoning_effort,
        Some(false)
    );
    assert_eq!(together_config.compat.supports_strict_mode, Some(false));

    let individual_ids = remaining_provider_models("qwen-token-plan-individual")
        .unwrap()
        .into_iter()
        .map(|model| model.common.model_ref.model.to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        individual_ids,
        [
            "deepseek-v4-flash-0731",
            "deepseek-v4-pro",
            "deepseek-v4-pro-0813",
            "glm-5.2",
            "qwen3.6-flash",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.8-max",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );

    for provider in [
        "xiaomi",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-sgp",
    ] {
        let ids = remaining_provider_models(provider)
            .unwrap()
            .into_iter()
            .map(|model| model.common.model_ref.model.to_string())
            .collect::<std::collections::BTreeSet<_>>();
        for removed in ["mimo-v2-flash", "mimo-v2-omni", "mimo-v2-pro"] {
            assert!(!ids.contains(removed), "{provider}/{removed}");
        }
        for replacement in ["mimo-v2.5", "mimo-v2.5-pro"] {
            assert!(ids.contains(replacement), "{provider}/{replacement}");
        }
    }

    let zai = pinned_model("zai-coding-cn", "glm-4.6v");
    assert_eq!(zai.common.pricing.default.input, MoneyRate::new(300_000));
    assert_eq!(zai.common.pricing.default.output, MoneyRate::new(900_000));
    let ApiModelConfig::OpenAiCompletions(zai_config) = zai.api else {
        panic!("Z.AI GLM-4.6V must use OpenAI Completions")
    };
    assert_eq!(
        zai_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::Zai)
    );
    assert_eq!(zai_config.compat.zai_tool_stream, Some(true));

    let copilot = pinned_model("github-copilot", "gpt-5.4");
    assert_eq!(
        copilot.common.headers.get("Copilot-Integration-Id"),
        Some(&Some("vscode-chat".into()))
    );
    assert_eq!(
        copilot.common.headers.get("Editor-Version"),
        Some(&Some("vscode/1.107.0".into()))
    );
}

/// Architecture v2 part 2 §5.1, §5.2, and §10.7; pinned Pi basis:
/// `packages/ai/test/zai-coding-plan-models.test.ts`,
/// "uses API-equivalent reference costs for Coding Plan models".
#[test]
fn zai_coding_plan_catalog_costs_match_new_pin_pi_exact() {
    for provider in ["zai", "zai-coding-cn"] {
        let model = pinned_model(provider, "glm-5.3");
        assert_eq!(
            model.common.pricing.default.input,
            MoneyRate::new(1_400_000)
        );
        assert_eq!(
            model.common.pricing.default.output,
            MoneyRate::new(4_400_000)
        );
        assert_eq!(
            model.common.pricing.default.cache_read,
            MoneyRate::new(260_000)
        );
        assert_eq!(model.common.pricing.default.cache_write, MoneyRate::new(0));
    }
}

#[test]
fn pi_ai_providers_all_registers_every_remaining_provider_send_and_local() {
    // Pi basis: packages/ai/src/providers/all.ts `builtinProviders`.
    let send = remaining_providers(ProviderInputs {
        http: Arc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let local = local_remaining_providers(LocalProviderInputs {
        http: Rc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    assert_eq!(send.len(), REMAINING_PROVIDER_IDS.len() + 1);
    assert_eq!(local.len(), REMAINING_PROVIDER_IDS.len() + 1);
    assert!(
        send.iter()
            .any(|item| item.descriptor.id == ProviderId::new("radius"))
    );
    assert!(
        local
            .iter()
            .any(|item| item.descriptor.id == ProviderId::new("radius"))
    );
}

#[test]
fn pi_ai_providers_all_registers_complete_pinned_order_send_and_local() {
    // Pi basis: packages/ai/src/providers/all.ts `builtinProviders`.
    let expected = [
        "amazon-bedrock",
        "ant-ling",
        "anthropic",
        "azure-openai-responses",
        "baseten",
        "cerebras",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "deepseek",
        "fireworks",
        "github-copilot",
        "google",
        "google-vertex",
        "groq",
        "huggingface",
        "kimi-coding",
        "minimax",
        "minimax-cn",
        "mistral",
        "moonshotai",
        "moonshotai-cn",
        "nvidia",
        "openai",
        "openai-codex",
        "opencode",
        "opencode-go",
        "openrouter",
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
    ];
    let send = builtin_providers(BuiltinProviderInputs {
        http: Arc::new(NoNetwork),
        bedrock: Arc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let local = local_builtin_providers(LocalBuiltinProviderInputs {
        http: Rc::new(NoNetwork),
        bedrock: Rc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    assert_eq!(
        send.iter()
            .map(|provider| provider.descriptor.id.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        local
            .iter()
            .map(|provider| provider.descriptor.id.as_str())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn remaining_provider_auth_is_api_family_scoped_send_and_local_pi_exact() {
    // Pi basis: packages/ai/src/api/anthropic-messages.ts,
    // google-generative-ai.ts, openai-completions.ts, and providers/opencode.ts.
    // One OpenCode credential is projected differently by each selected API
    // family; the provider leaf must not pre-emptively turn all keys into
    // bearer authorization.
    let send = agentprism_opencode::provider(agentprism_provider_common::ProviderInputs {
        http: Arc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let send_context = Arc::new(MapAuthContext::new(
        BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "family-key".to_owned())]),
        Vec::<String>::new(),
    ));
    for (api, header, expected, retains_key) in [
        ("anthropic-messages", "x-api-key", "family-key", false),
        ("google-generative-ai", "x-goog-api-key", "family-key", true),
        (
            "openai-completions",
            "authorization",
            "Bearer family-key",
            false,
        ),
        (
            "openai-responses",
            "authorization",
            "Bearer family-key",
            false,
        ),
    ] {
        let model = send
            .catalog
            .snapshot()
            .iter()
            .find(|model| model.api.api_id() == ApiId::new(api))
            .unwrap()
            .clone();
        let mut request = ResolveAuthRequest::isolated(send.descriptor.clone(), Some(model));
        request.auth_context = send_context.clone();
        let resolved =
            futures_executor::block_on(send.auth.resolve(request, CancellationToken::new()))
                .unwrap()
                .unwrap();
        assert_eq!(resolved.headers[header], expected, "{api}");
        assert_eq!(resolved.api_key.is_some(), retains_key, "{api}");
        if api != "openai-completions" && api != "openai-responses" {
            assert!(!resolved.headers.contains_key("authorization"));
        }
    }

    let local =
        agentprism_opencode::local_provider(agentprism_provider_common::LocalProviderInputs {
            http: Rc::new(NoNetwork),
            environment: BTreeMap::new(),
        })
        .unwrap();
    let local_context = Rc::new(MapAuthContext::new(
        BTreeMap::from([("OPENCODE_API_KEY".to_owned(), "local-family-key".to_owned())]),
        Vec::<String>::new(),
    ));
    for (api, header, expected) in [
        ("anthropic-messages", "x-api-key", "local-family-key"),
        ("google-generative-ai", "x-goog-api-key", "local-family-key"),
        (
            "openai-completions",
            "authorization",
            "Bearer local-family-key",
        ),
        (
            "openai-responses",
            "authorization",
            "Bearer local-family-key",
        ),
    ] {
        let model = local
            .catalog
            .snapshot()
            .iter()
            .find(|model| model.api.api_id() == ApiId::new(api))
            .unwrap()
            .clone();
        let mut request = LocalResolveAuthRequest::isolated(local.descriptor.clone(), Some(model));
        request.auth_context = local_context.clone();
        let resolved =
            futures_executor::block_on(local.auth.resolve(request, CancellationToken::new()))
                .unwrap()
                .unwrap();
        assert_eq!(resolved.headers[header], expected, "{api}");
    }
}
