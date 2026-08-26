//! Pinned all-provider aggregation conformance.

use agentprism_ai::*;
use agentprism_providers_all::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
fn provider_dependency_boundaries_are_explicit_pi_equivalent() {
    // Pi basis: packages/ai/test/lazy-module-load.test.ts. Architecture v2
    // part 1 §2.2 replaces runtime module-load observations with separate leaf
    // crates and one explicit all-providers aggregation crate.
    let core_manifest = include_str!("../../../crates/agentprism-ai/Cargo.toml");
    for provider in [
        "agentprism-anthropic",
        "agentprism-bedrock",
        "agentprism-google",
        "agentprism-openai",
        "agentprism-providers-all",
    ] {
        assert!(
            !core_manifest.contains(provider),
            "the core crate must not pull in {provider}"
        );
    }

    let anthropic_manifest = include_str!("../../agentprism-anthropic/Cargo.toml");
    assert!(anthropic_manifest.contains("agentprism-ai"));
    for unrelated in [
        "agentprism-bedrock",
        "agentprism-google",
        "agentprism-openai",
        "agentprism-providers-all",
    ] {
        assert!(
            !anthropic_manifest.contains(unrelated),
            "the Anthropic leaf must not pull in {unrelated}"
        );
    }

    let all_manifest = include_str!("../Cargo.toml");
    for provider in [
        "agentprism-anthropic",
        "agentprism-bedrock",
        "agentprism-google",
        "agentprism-openai",
    ] {
        assert!(
            all_manifest.contains(provider),
            "the explicit aggregator must include {provider}"
        );
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

fn all_published_models() -> Vec<ModelDescriptor> {
    let mut models = Vec::new();
    models.extend(agentprism_anthropic::anthropic_models().expect("Anthropic catalog"));
    models.extend(agentprism_bedrock::bedrock_models());
    models.extend(agentprism_deepseek::models().expect("DeepSeek catalog"));
    models.extend(agentprism_google::google_models().expect("Google catalog"));
    models.extend(agentprism_google_vertex::models().expect("Google Vertex catalog"));
    models.extend(agentprism_mistral::mistral_models().expect("Mistral catalog"));
    models.extend(agentprism_openai::openai_models().expect("OpenAI catalog"));
    models.extend(agentprism_openai_codex::models().expect("OpenAI Codex catalog"));
    models.extend(agentprism_openrouter::models().expect("OpenRouter catalog"));
    for provider in REMAINING_PROVIDER_IDS {
        models.extend(remaining_provider_models(provider).unwrap_or_else(|error| {
            panic!("{provider} catalog failed: {error}");
        }));
    }
    models
}

fn published_model(provider: &str, model: &str) -> ModelDescriptor {
    all_published_models()
        .into_iter()
        .find(|candidate| candidate.common.model_ref == ModelRef::new(provider, model))
        .unwrap_or_else(|| panic!("missing pinned model {provider}/{model}"))
}

fn supported_levels<T>(reasoning: bool, levels: &ThinkingLevelMap<T>) -> Vec<&'static str> {
    if !reasoning {
        return vec!["off"];
    }
    let ordinary =
        |entry: &Option<LevelSupport<T>>| !matches!(entry, Some(LevelSupport::Unsupported));
    let extended = |entry: &Option<LevelSupport<T>>| {
        matches!(entry, Some(LevelSupport::Disabled | LevelSupport::Value(_)))
    };
    [
        ("off", ordinary(&levels.off)),
        ("minimal", ordinary(&levels.minimal)),
        ("low", ordinary(&levels.low)),
        ("medium", ordinary(&levels.medium)),
        ("high", ordinary(&levels.high)),
        ("xhigh", extended(&levels.xhigh)),
        ("max", extended(&levels.max)),
    ]
    .into_iter()
    .filter_map(|(name, supported)| supported.then_some(name))
    .collect()
}

fn model_supported_levels(model: &ModelDescriptor) -> Vec<&'static str> {
    match &model.api {
        ApiModelConfig::OpenAiCompletions(config) => {
            supported_levels(model.common.reasoning, &config.thinking_levels)
        }
        ApiModelConfig::OpenAiResponses(config) | ApiModelConfig::OpenAiCodexResponses(config) => {
            supported_levels(model.common.reasoning, &config.thinking_levels)
        }
        ApiModelConfig::AnthropicMessages(config) => {
            supported_levels(model.common.reasoning, &config.thinking_levels)
        }
        ApiModelConfig::BedrockConverse(config) => {
            supported_levels(model.common.reasoning, &config.thinking_levels)
        }
        other => panic!(
            "supports-xhigh fixture unexpectedly uses {}",
            other.api_id()
        ),
    }
}

/// Architecture v2 part 2 §5.1, §5.2, and §10.7; pinned Pi basis:
/// `packages/ai/test/anthropic-adaptive-thinking-models.test.ts` enumerates
/// every built-in Anthropic Messages catalog, not just Anthropic's own leaf.
#[test]
fn anthropic_adaptive_thinking_catalog_matrix_pi_exact() {
    let mut flagged = all_published_models()
        .into_iter()
        .filter_map(|model| {
            let ApiModelConfig::AnthropicMessages(config) = &model.api else {
                return None;
            };
            (config.compat.force_adaptive_thinking == Some(true)).then(|| {
                format!(
                    "{}/{}",
                    model.common.model_ref.provider, model.common.model_ref.model
                )
            })
        })
        .collect::<Vec<_>>();
    flagged.sort();

    for expected in [
        "anthropic/claude-fable-5",
        "anthropic/claude-opus-4-8",
        "anthropic/claude-opus-5",
        "anthropic/claude-sonnet-5",
        "cloudflare-ai-gateway/claude-fable-5",
        "kimi-coding/kimi-for-coding",
        "kimi-coding/k3",
        "kimi-coding/kimi-for-coding-highspeed",
        "opencode/claude-opus-4-8",
        "opencode/claude-opus-5",
        "vercel-ai-gateway/anthropic/claude-opus-4.8",
        "vercel-ai-gateway/anthropic/claude-opus-5",
        "vercel-ai-gateway/anthropic/claude-sonnet-5",
    ] {
        assert!(
            flagged.iter().any(|actual| actual == expected),
            "{expected}"
        );
    }
    for model in &flagged {
        let normalized = model.replace('.', "-");
        assert!(
            normalized.starts_with("kimi-coding/")
                || normalized.contains("opus-4-6")
                || normalized.contains("opus-4-7")
                || normalized.contains("opus-4-8")
                || normalized.contains("opus-5")
                || normalized.contains("sonnet-4-6")
                || normalized.contains("sonnet-5")
                || normalized.contains("fable-5"),
            "unexpected adaptive-thinking model: {model}"
        );
    }
}

/// Architecture v2 part 2 §5.1 and §10.7; pinned Pi basis:
/// `packages/ai/test/supports-xhigh.test.ts`, scenario-for-scenario.
#[test]
fn supported_thinking_levels_catalog_matrix_pi_exact() {
    for (provider, model, expected) in [
        (
            "openai",
            "gpt-5.6-sol",
            &["off", "low", "medium", "high", "xhigh", "max"][..],
        ),
        (
            "openai",
            "gpt-5.6-terra",
            &["off", "low", "medium", "high", "xhigh", "max"],
        ),
        (
            "openai",
            "gpt-5.6-luna",
            &["off", "low", "medium", "high", "xhigh", "max"],
        ),
        ("openai", "gpt-5.5-pro", &["medium", "high", "xhigh"]),
        (
            "openrouter",
            "openai/gpt-5.5-pro",
            &["medium", "high", "xhigh"],
        ),
        (
            "deepseek",
            "deepseek-v4-flash",
            &["off", "low", "high", "max"],
        ),
        (
            "opencode-go",
            "deepseek-v4-flash",
            &["off", "low", "high", "max"],
        ),
        ("opencode-go", "kimi-k2.6", &["off", "high"]),
        (
            "moonshotai",
            "kimi-k2.7-code",
            &["minimal", "low", "medium", "high"],
        ),
        (
            "moonshotai-cn",
            "kimi-k2.7-code",
            &["minimal", "low", "medium", "high"],
        ),
        ("moonshotai", "kimi-k3", &["low", "high", "max"]),
        ("moonshotai-cn", "kimi-k3", &["low", "high", "max"]),
        ("kimi-coding", "k3", &["low", "high", "max"]),
        ("opencode", "grok-build-0.1", &["high"]),
        (
            "openrouter",
            "deepseek/deepseek-v4-flash",
            &["off", "high", "xhigh"],
        ),
        ("xai", "grok-4.6", &["low", "medium", "high", "xhigh"]),
    ] {
        assert_eq!(
            model_supported_levels(&published_model(provider, model)),
            expected,
            "{provider}/{model}"
        );
    }

    for (provider, model) in [
        ("anthropic", "claude-opus-4-6"),
        ("anthropic", "claude-sonnet-4-6"),
        ("openrouter", "anthropic/claude-opus-4.6"),
    ] {
        let levels = model_supported_levels(&published_model(provider, model));
        assert!(levels.contains(&"max"), "{provider}/{model}");
        assert!(!levels.contains(&"xhigh"), "{provider}/{model}");
    }
    for (provider, model) in [
        ("anthropic", "claude-opus-4-8"),
        ("anthropic", "claude-opus-5"),
        ("anthropic", "claude-sonnet-5"),
        ("amazon-bedrock", "global.anthropic.claude-opus-5"),
    ] {
        let levels = model_supported_levels(&published_model(provider, model));
        assert!(levels.contains(&"xhigh"), "{provider}/{model}");
        assert!(levels.contains(&"max"), "{provider}/{model}");
    }
    for (provider, model) in [
        ("anthropic", "claude-fable-5"),
        ("amazon-bedrock", "global.anthropic.claude-fable-5"),
    ] {
        let levels = model_supported_levels(&published_model(provider, model));
        assert!(levels.contains(&"xhigh"), "{provider}/{model}");
        assert!(levels.contains(&"max"), "{provider}/{model}");
        assert!(!levels.contains(&"off"), "{provider}/{model}");
    }
    let legacy_sonnet = model_supported_levels(&published_model("anthropic", "claude-sonnet-4-5"));
    assert!(!legacy_sonnet.contains(&"xhigh"));
    assert!(!legacy_sonnet.contains(&"max"));

    for model in [
        "gpt-5.4",
        "gpt-5.5",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ] {
        assert!(
            model_supported_levels(&published_model("openai-codex", model)).contains(&"xhigh"),
            "openai-codex/{model}"
        );
    }
}

/// Architecture v2 part 2 §3, §5, §10.1, and §10.8; pinned Pi basis:
/// `packages/ai/test/stream.test.ts`. The provider rows are catalog/API
/// composition coverage; API-family event and wire semantics remain covered
/// by the family-specific named conformance tests mapped alongside this test.
#[test]
fn live_provider_stream_matrix_catalog_composition_pi_exact() {
    for (provider, model) in [
        ("google", "gemini-2.5-flash"),
        ("google-vertex", "gemini-3-flash-preview"),
        ("deepseek", "deepseek-v4-flash"),
        ("openai", "gpt-5.4"),
        ("anthropic", "claude-haiku-4-5"),
        ("azure-openai-responses", "gpt-4o-mini"),
        ("xai", "grok-4.3"),
        ("groq", "openai/gpt-oss-20b"),
        ("cerebras", "gpt-oss-120b"),
        ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
        (
            "cloudflare-ai-gateway",
            "workers-ai/@cf/moonshotai/kimi-k2.6",
        ),
        ("cloudflare-ai-gateway", "gpt-5.1"),
        ("cloudflare-ai-gateway", "claude-sonnet-4.5"),
        ("huggingface", "moonshotai/Kimi-K2.5"),
        ("together", "moonshotai/Kimi-K2.6"),
        ("baseten", "zai-org/GLM-5.2"),
        ("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
        ("openrouter", "z-ai/glm-4.5v"),
        ("vercel-ai-gateway", "google/gemini-2.5-flash"),
        ("vercel-ai-gateway", "anthropic/claude-opus-4.5"),
        ("vercel-ai-gateway", "openai/gpt-5.1-codex-max"),
        ("zai", "glm-5.2"),
        ("mistral", "devstral-medium-latest"),
        ("mistral", "mistral-small-2603"),
        ("mistral", "pixtral-12b"),
        ("minimax", "MiniMax-M2.7"),
        ("kimi-coding", "kimi-for-coding"),
        ("xiaomi", "mimo-v2.5-pro"),
        ("xiaomi-token-plan-cn", "mimo-v2.5-pro"),
        ("xiaomi-token-plan-ams", "mimo-v2.5-pro"),
        ("xiaomi-token-plan-sgp", "mimo-v2.5-pro"),
        ("qwen-token-plan", "qwen3.7-max"),
        ("qwen-token-plan-individual", "qwen3.8-max"),
        ("qwen-token-plan-cn", "qwen3.7-max"),
        ("ant-ling", "Ling-2.6-flash"),
        ("ant-ling", "Ring-2.6-1T"),
        ("anthropic", "claude-sonnet-4-6"),
        ("anthropic", "claude-opus-4-6"),
        ("github-copilot", "gpt-5.3-codex"),
        ("github-copilot", "gpt-5-mini"),
        ("github-copilot", "claude-sonnet-4.6"),
        ("openai-codex", "gpt-5.4"),
        ("openai-codex", "gpt-5.5"),
        (
            "amazon-bedrock",
            "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
        ),
        ("amazon-bedrock", "global.anthropic.claude-opus-4-6-v1"),
    ] {
        let descriptor = published_model(provider, model);
        assert_eq!(descriptor.common.model_ref, ModelRef::new(provider, model));
        assert!(
            descriptor
                .common
                .modalities
                .output
                .contains(&Modality::Text)
        );
        assert!(!descriptor.api.api_id().as_str().is_empty());
    }
}

/// Architecture v2 part 2 §3.6, §5.1, and §10.8; pinned Pi basis:
/// `packages/ai/test/cache-retention.test.ts`, the six OpenCode rows that
/// explicitly suppress long prompt-cache fields.
#[test]
fn opencode_long_cache_retention_exclusion_matrix_pi_exact() {
    for (provider, model) in [
        ("opencode", "deepseek-v4-flash"),
        ("opencode", "deepseek-v4-pro"),
        ("opencode", "kimi-k2.5"),
        ("opencode", "kimi-k2.6"),
        ("opencode", "minimax-m2.7"),
        ("opencode-go", "kimi-k2.6"),
    ] {
        let descriptor = published_model(provider, model);
        let ApiModelConfig::OpenAiCompletions(config) = descriptor.api else {
            panic!("{provider}/{model} must use OpenAI Completions")
        };
        assert_eq!(
            config.compat.supports_long_cache_retention,
            Some(false),
            "{provider}/{model}"
        );
    }
}

/// Architecture v2 part 2 §5.1, §5.3, §5.4, and §10.7; pinned Pi basis:
/// `packages/ai/test/model-data-validation.test.ts`. Rust embeds each
/// provider-owned shard at compile time, while this exercises the equivalent
/// candidate-schema failures before a typed catalog can be published.
#[test]
fn published_catalog_validation_rejects_malformed_candidates_pi_exact() {
    let parse = |source: &str| {
        agentprism_openai::parse_openai_published_catalog(
            source,
            "test-provider",
            "openai-completions",
        )
    };
    assert!(
        parse("not-json")
            .unwrap_err()
            .to_string()
            .contains("invalid published catalog")
    );
    assert!(
        parse(r#"{}"#)
            .unwrap_err()
            .to_string()
            .contains("catalog omits openai-completions")
    );

    let model = |provider: &str, api: &str, id: Option<&str>| {
        let mut value = serde_json::json!({
            "name": "Model A",
            "api": api,
            "provider": provider,
            "baseUrl": "https://example.test/v1",
            "reasoning": false,
            "input": ["text"],
            "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 1000,
            "maxTokens": 100
        });
        if let Some(id) = id {
            value["id"] = serde_json::Value::String(id.to_owned());
        }
        serde_json::json!({"openai-completions": {"model-a": value}}).to_string()
    };
    assert!(
        parse(&model(
            "wrong-provider",
            "openai-completions",
            Some("model-a")
        ))
        .unwrap_err()
        .to_string()
        .contains("expected test-provider")
    );
    assert!(
        parse(&model(
            "test-provider",
            "anthropic-messages",
            Some("model-a")
        ))
        .unwrap_err()
        .to_string()
        .contains("does not use openai-completions")
    );
    assert!(
        parse(&model("test-provider", "openai-completions", None))
            .unwrap_err()
            .to_string()
            .contains("catalog field id is not a string")
    );
}

static NEXT_MODEL_DATA_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct ModelDataFixture {
    root: PathBuf,
    structure: agentprism_provider_common::ModelDataStructure,
    values: serde_json::Value,
}

impl ModelDataFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "agentprism-model-data-{}-{}",
            std::process::id(),
            NEXT_MODEL_DATA_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create model-data fixture");
        let structure = BTreeMap::from([(
            "test-provider".into(),
            BTreeMap::from([("model-a".into(), "openai-completions".into())]),
        )]);
        let values = serde_json::json!({
            "model-a": {
                "id": "model-a",
                "name": "Model A",
                "api": "openai-completions",
                "provider": "test-provider",
                "baseUrl": "https://example.test/v1",
                "reasoning": false,
                "input": ["text"],
                "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 1000,
                "maxTokens": 100
            }
        });
        let fixture = Self {
            root,
            structure,
            values,
        };
        fixture.write(
            &fixture.values,
            agentprism_provider_common::MODEL_DATA_SCHEMA_VERSION,
            "openai-completions",
        );
        fixture
    }

    fn write(&self, values: &serde_json::Value, schema_version: u32, api_group: &str) {
        let filename = "test-provider.json";
        let groups = BTreeMap::from([(api_group, values)]);
        let content = format!("{}\n", serde_json::to_string(&groups).unwrap());
        fs::write(self.root.join(filename), &content).expect("write provider data");
        let mut manifest = agentprism_provider_common::create_model_data_manifest(
            &self.structure,
            &BTreeMap::from([(filename.into(), content)]),
            "2026-07-23T10:00:00.000Z",
        );
        manifest.schema_version = schema_version;
        fs::write(
            self.root
                .join(agentprism_provider_common::MODEL_DATA_MANIFEST_FILE),
            format!("{}\n", serde_json::to_string(&manifest).unwrap()),
        )
        .expect("write model-data manifest");
    }

    fn validate(&self) -> Result<(), agentprism_provider_common::ModelDataValidationError> {
        agentprism_provider_common::validate_model_data_directory(&self.structure, &self.root)
    }

    fn manifest_path(&self) -> PathBuf {
        self.root
            .join(agentprism_provider_common::MODEL_DATA_MANIFEST_FILE)
    }
}

impl Drop for ModelDataFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn rewrite_manifest_field(path: &Path, field: &str, value: serde_json::Value) {
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(path).expect("read manifest")).expect("manifest JSON");
    manifest[field] = value;
    fs::write(path, format!("{}\n", manifest)).expect("rewrite manifest");
}

/// Architecture v2 part 2 §5.1, §5.3, §5.4, and §10.7; pinned Pi
/// basis: every scenario in `packages/ai/test/model-data-validation.test.ts`.
#[test]
fn native_model_data_validation_scenarios_pi_exact() {
    let error = agentprism_provider_common::assert_exact_model_ids(
        "qwen-token-plan-individual",
        ["model-a", "model-b"],
        ["model-a"],
    )
    .unwrap_err();
    assert!(error.to_string().contains("missing: model-b"));
    let error = agentprism_provider_common::assert_exact_model_ids(
        "test-provider",
        ["model-a"],
        ["model-a", "model-b"],
    )
    .unwrap_err();
    assert!(error.to_string().contains("extra: model-b"));

    let fixture = ModelDataFixture::new();
    fixture.validate().expect("valid API-grouped model data");
    let missing = fixture.root.join("missing");
    let error =
        agentprism_provider_common::validate_model_data_directory(&fixture.structure, &missing)
            .unwrap_err();
    assert!(error.to_string().contains("does not exist"));

    for (field, replacement, expected) in [
        ("id", "wrong-id", "has id"),
        ("provider", "wrong-provider", "has provider"),
        ("api", "anthropic-messages", "has api"),
    ] {
        let fixture = ModelDataFixture::new();
        let mut values = fixture.values.clone();
        values["model-a"][field] = replacement.into();
        fixture.write(
            &values,
            agentprism_provider_common::MODEL_DATA_SCHEMA_VERSION,
            "openai-completions",
        );
        assert!(
            fixture
                .validate()
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
    }

    let fixture = ModelDataFixture::new();
    fixture.write(
        &fixture.values,
        agentprism_provider_common::MODEL_DATA_SCHEMA_VERSION,
        "anthropic-messages",
    );
    assert!(
        fixture
            .validate()
            .unwrap_err()
            .to_string()
            .contains("grouped under API")
    );

    let fixture = ModelDataFixture::new();
    let filename = "test-provider.json";
    let content = format!(
        "{}\n",
        serde_json::json!({
            "openai-completions": fixture.values.clone(),
            "anthropic-messages": fixture.values.clone(),
        })
    );
    fs::write(fixture.root.join(filename), &content).unwrap();
    let manifest = agentprism_provider_common::create_model_data_manifest(
        &fixture.structure,
        &BTreeMap::from([(filename.into(), content)]),
        "2026-07-23T10:00:00.000Z",
    );
    fs::write(
        fixture.manifest_path(),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert!(
        fixture
            .validate()
            .unwrap_err()
            .to_string()
            .contains("more than one API group")
    );

    let fixture = ModelDataFixture::new();
    fs::write(fixture.root.join("test-provider.json"), "{}\n").unwrap();
    let error = fixture.validate().unwrap_err().to_string();
    assert!(error.contains("manifest hash") || error.contains("model IDs"));

    let fixture = ModelDataFixture::new();
    fixture.write(
        &fixture.values,
        agentprism_provider_common::MODEL_DATA_SCHEMA_VERSION + 1,
        "openai-completions",
    );
    assert!(
        fixture
            .validate()
            .unwrap_err()
            .to_string()
            .contains("model data schema")
    );
    rewrite_manifest_field(&fixture.manifest_path(), "structureHash", "stale".into());
    assert!(
        fixture
            .validate()
            .unwrap_err()
            .to_string()
            .contains("generation stamp")
    );

    let fixture = ModelDataFixture::new();
    rewrite_manifest_field(&fixture.manifest_path(), "generatedAt", "invalid".into());
    assert!(
        fixture
            .validate()
            .unwrap_err()
            .to_string()
            .contains("generation timestamp")
    );

    let error = agentprism_provider_common::validate_model_shard_inventory(
        ["test-provider", "missing"],
        ["test-provider.models.rs"],
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("aggregator and provider shards do not match")
    );
}

/// Architecture v2 part 2 §5.1, §5.2, and §10.7; pinned Pi basis:
/// `packages/ai/test/providers.test.ts` built-in constrained-sampling and
/// Kimi pricing scenarios.
#[test]
fn provider_control_plane_catalog_metadata_matrix_pi_exact() {
    let gpt_4o = published_model("openai", "gpt-4o");
    let ApiModelConfig::OpenAiResponses(gpt_4o_config) = gpt_4o.api else {
        panic!("OpenAI GPT-4o must use Responses")
    };
    assert_eq!(gpt_4o_config.compat.supports_strict_mode, Some(true));
    assert_eq!(gpt_4o_config.compat.supports_openai_grammar_tools, None);

    let gpt_54 = published_model("openai", "gpt-5.4");
    let ApiModelConfig::OpenAiResponses(gpt_54_config) = gpt_54.api else {
        panic!("OpenAI GPT-5.4 must use Responses")
    };
    assert_eq!(gpt_54_config.compat.supports_strict_mode, Some(true));
    assert_eq!(
        gpt_54_config.compat.supports_openai_grammar_tools,
        Some(true)
    );

    let haiku = published_model("anthropic", "claude-haiku-4-5");
    let ApiModelConfig::AnthropicMessages(haiku_config) = haiku.api else {
        panic!("Anthropic Haiku must use Messages")
    };
    assert_eq!(haiku_config.compat.supports_strict_tools, Some(true));

    for provider in ["moonshotai", "moonshotai-cn"] {
        let kimi = published_model(provider, "kimi-k3");
        assert_eq!(kimi.common.pricing.default.input, MoneyRate::new(3_000_000));
        assert_eq!(
            kimi.common.pricing.default.output,
            MoneyRate::new(15_000_000)
        );
        assert_eq!(
            kimi.common.pricing.default.cache_read,
            MoneyRate::new(300_000)
        );
        assert_eq!(kimi.common.pricing.default.cache_write, MoneyRate::new(0));
    }
    for (model, input, output, cache_read) in [
        ("k3", 3_000_000, 15_000_000, 300_000),
        ("kimi-for-coding-highspeed", 1_900_000, 8_000_000, 380_000),
    ] {
        let kimi = published_model("kimi-coding", model);
        assert_eq!(kimi.common.pricing.default.input, MoneyRate::new(input));
        assert_eq!(kimi.common.pricing.default.output, MoneyRate::new(output));
        assert_eq!(
            kimi.common.pricing.default.cache_read,
            MoneyRate::new(cache_read)
        );
        assert_eq!(kimi.common.pricing.default.cache_write, MoneyRate::new(0));
    }
}

/// Architecture v2 part 2 §6.1 and §10.7; pinned Pi basis:
/// `packages/ai/test/env-api-keys.test.ts`. Environment lookup is performed
/// through the injected auth context, never by reading process globals.
#[test]
fn environment_api_key_alias_matrix_pi_exact() {
    let inputs = |environment| agentprism_provider_common::ProviderInputs {
        http: Arc::new(NoNetwork),
        environment,
    };

    let copilot = agentprism_github_copilot::provider(inputs(BTreeMap::new()))
        .expect("GitHub Copilot registration");
    let mut generic_request = ResolveAuthRequest::isolated(copilot.descriptor.clone(), None);
    generic_request.auth_context = Arc::new(MapAuthContext::new(
        BTreeMap::from([
            ("GH_TOKEN".to_owned(), "gh-token".to_owned()),
            ("GITHUB_TOKEN".to_owned(), "github-token".to_owned()),
        ]),
        Vec::<String>::new(),
    ));
    assert!(
        futures_executor::block_on(
            copilot
                .auth
                .resolve(generic_request, CancellationToken::new())
        )
        .expect("generic GitHub variables are a valid empty lookup")
        .is_none()
    );

    let mut copilot_request = ResolveAuthRequest::isolated(copilot.descriptor.clone(), None);
    copilot_request.auth_context = Arc::new(MapAuthContext::new(
        BTreeMap::from([
            (
                "COPILOT_GITHUB_TOKEN".to_owned(),
                "copilot-token".to_owned(),
            ),
            ("GH_TOKEN".to_owned(), "gh-token".to_owned()),
            ("GITHUB_TOKEN".to_owned(), "github-token".to_owned()),
        ]),
        Vec::<String>::new(),
    ));
    let copilot_auth = futures_executor::block_on(
        copilot
            .auth
            .resolve(copilot_request, CancellationToken::new()),
    )
    .expect("Copilot environment resolution")
    .expect("COPILOT_GITHUB_TOKEN resolves");
    assert_eq!(copilot_auth.source.0, "COPILOT_GITHUB_TOKEN");

    let zai = agentprism_zai_coding_cn::provider(inputs(BTreeMap::new()))
        .expect("Z.AI Coding CN registration");
    let mut zai_request = ResolveAuthRequest::isolated(zai.descriptor.clone(), None);
    zai_request.auth_context = Arc::new(MapAuthContext::new(
        BTreeMap::from([(
            "ZAI_CODING_CN_API_KEY".to_owned(),
            "zai-coding-cn-token".to_owned(),
        )]),
        Vec::<String>::new(),
    ));
    let zai_auth =
        futures_executor::block_on(zai.auth.resolve(zai_request, CancellationToken::new()))
            .expect("Z.AI environment resolution")
            .expect("ZAI_CODING_CN_API_KEY resolves");
    assert_eq!(zai_auth.source.0, "ZAI_CODING_CN_API_KEY");

    let anthropic = agentprism_anthropic::anthropic_provider(Arc::new(NoNetwork))
        .expect("Anthropic registration");
    let mut anthropic_request = ResolveAuthRequest::isolated(anthropic.descriptor.clone(), None);
    anthropic_request.auth_context = Arc::new(MapAuthContext::new(
        BTreeMap::from([
            ("ANTHROPIC_AUTH_TOKEN".to_owned(), "auth-token".to_owned()),
            ("ANTHROPIC_OAUTH_TOKEN".to_owned(), "oauth-token".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), "api-key".to_owned()),
        ]),
        Vec::<String>::new(),
    ));
    let anthropic_auth = futures_executor::block_on(
        anthropic
            .auth
            .resolve(anthropic_request, CancellationToken::new()),
    )
    .expect("Anthropic environment resolution")
    .expect("Anthropic environment resolves");
    assert_eq!(anthropic_auth.source.0, "ANTHROPIC_AUTH_TOKEN");
    assert!(anthropic_auth.api_key.is_none());
    assert_eq!(anthropic_auth.headers["authorization"], "Bearer auth-token");
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

fn unsupported() -> Option<LevelSupport<OpenAiThinkingValue>> {
    Some(LevelSupport::Unsupported)
}

fn effort(value: &str) -> Option<LevelSupport<OpenAiThinkingValue>> {
    Some(LevelSupport::Value(OpenAiThinkingValue::Effort(
        value.into(),
    )))
}

/// Architecture v2 part 2 §5.1, §5.2, and §10.7; pinned Pi basis:
/// `packages/ai/test/baseten-models.test.ts`.
#[test]
fn baseten_models_catalog_and_reasoning_contract_pi_exact() {
    let glm = pinned_model("baseten", "zai-org/GLM-5.2");
    assert_eq!(glm.common.display_name, "GLM 5.2");
    assert_eq!(
        glm.common.base_url.as_str(),
        "https://inference.baseten.co/v1"
    );
    assert!(glm.common.reasoning);
    assert_eq!(
        glm.common.modalities.input,
        BTreeSet::from([Modality::Text, Modality::Image])
    );
    assert_eq!(glm.common.limits.context_window, 1_048_576);
    assert_eq!(glm.common.limits.max_output_tokens, 262_144);
    assert_eq!(glm.common.pricing.default.input, MoneyRate::new(1_400_000));
    assert_eq!(glm.common.pricing.default.output, MoneyRate::new(4_400_000));
    assert_eq!(
        glm.common.pricing.default.cache_read,
        MoneyRate::new(300_000)
    );
    assert_eq!(glm.common.pricing.default.cache_write, MoneyRate::new(0));
    let ApiModelConfig::OpenAiCompletions(glm_config) = glm.api else {
        panic!("Baseten GLM 5.2 must use OpenAI Completions")
    };
    assert_eq!(
        glm_config.thinking_levels,
        ThinkingLevelMap {
            off: effort("none"),
            minimal: unsupported(),
            low: unsupported(),
            medium: unsupported(),
            high: effort("high"),
            xhigh: unsupported(),
            max: effort("max"),
        }
    );
    assert_eq!(glm_config.compat.supports_store, Some(false));
    assert_eq!(glm_config.compat.supports_developer_role, Some(false));
    assert_eq!(glm_config.compat.supports_reasoning_effort, Some(true));
    assert_eq!(glm_config.compat.supports_usage_in_streaming, Some(true));
    assert_eq!(
        glm_config.compat.max_tokens_field,
        Some(MaxTokensField::MaxTokens)
    );
    assert_eq!(glm_config.compat.supports_strict_mode, Some(true));
    assert_eq!(glm_config.compat.supports_long_cache_retention, Some(false));
    assert_eq!(
        glm_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::Baseten)
    );
    assert_eq!(
        serde_json::to_value(glm_config.compat.chat_template_args).unwrap(),
        serde_json::json!({"enable_thinking":{"$var":"thinking.enabled"}})
    );

    let kimi = pinned_model("baseten", "moonshotai/Kimi-K2.6");
    let ApiModelConfig::OpenAiCompletions(kimi_config) = kimi.api else {
        panic!("Baseten Kimi K2.6 must use OpenAI Completions")
    };
    assert_eq!(
        kimi_config.thinking_levels,
        ThinkingLevelMap {
            off: effort("off"),
            minimal: unsupported(),
            low: unsupported(),
            medium: unsupported(),
            high: effort("high"),
            xhigh: unsupported(),
            max: unsupported(),
        }
    );
    assert_eq!(kimi_config.compat.supports_reasoning_effort, Some(false));
    assert_eq!(
        kimi_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::Baseten)
    );
}

/// Architecture v2 part 2 §5.1, §5.2, and §10.7; pinned Pi basis:
/// `packages/ai/test/together-models.test.ts`.
#[test]
fn together_models_catalog_and_reasoning_contract_pi_exact() {
    let kimi = pinned_model("together", "moonshotai/Kimi-K2.6");
    assert_eq!(kimi.common.base_url.as_str(), "https://api.together.ai/v1");
    assert_eq!(kimi.common.limits.context_window, 262_144);
    assert_eq!(kimi.common.limits.max_output_tokens, 131_000);
    assert_eq!(kimi.common.pricing.default.input, MoneyRate::new(1_200_000));
    assert_eq!(
        kimi.common.pricing.default.output,
        MoneyRate::new(4_500_000)
    );
    assert_eq!(
        kimi.common.pricing.default.cache_read,
        MoneyRate::new(200_000)
    );
    let ApiModelConfig::OpenAiCompletions(kimi_config) = kimi.api else {
        panic!("Together Kimi K2.6 must use OpenAI Completions")
    };
    assert_eq!(kimi_config.compat.supports_store, Some(false));
    assert_eq!(kimi_config.compat.supports_developer_role, Some(false));
    assert_eq!(kimi_config.compat.supports_reasoning_effort, Some(false));
    assert_eq!(
        kimi_config.compat.max_tokens_field,
        Some(MaxTokensField::MaxTokens)
    );
    assert_eq!(
        kimi_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::Together)
    );
    assert_eq!(kimi_config.compat.supports_strict_mode, Some(false));
    assert_eq!(
        kimi_config.compat.supports_long_cache_retention,
        Some(false)
    );

    let gpt_oss = pinned_model("together", "openai/gpt-oss-120b");
    let ApiModelConfig::OpenAiCompletions(gpt_oss_config) = gpt_oss.api else {
        panic!("Together GPT OSS must use OpenAI Completions")
    };
    assert_eq!(gpt_oss_config.compat.supports_reasoning_effort, Some(true));
    assert_eq!(
        gpt_oss_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::OpenAi)
    );
    assert_eq!(gpt_oss_config.thinking_levels.off, unsupported());
    assert_eq!(gpt_oss_config.thinking_levels.low, effort("low"));
    assert_eq!(gpt_oss_config.thinking_levels.medium, effort("medium"));
    assert_eq!(gpt_oss_config.thinking_levels.high, effort("high"));

    let deepseek = pinned_model("together", "deepseek-ai/DeepSeek-V4-Pro");
    let ApiModelConfig::OpenAiCompletions(deepseek_config) = deepseek.api else {
        panic!("Together DeepSeek V4 Pro must use OpenAI Completions")
    };
    assert_eq!(deepseek_config.compat.supports_reasoning_effort, Some(true));
    assert_eq!(
        deepseek_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::Together)
    );
    assert_eq!(deepseek_config.thinking_levels.high, effort("high"));

    let minimax = pinned_model("together", "MiniMaxAI/MiniMax-M2.7");
    let ApiModelConfig::OpenAiCompletions(minimax_config) = minimax.api else {
        panic!("Together MiniMax M2.7 must use OpenAI Completions")
    };
    assert_eq!(minimax_config.compat.supports_reasoning_effort, Some(false));
    assert_eq!(minimax_config.compat.thinking_format, None);
}

/// Architecture v2 part 2 §5.1, §5.2, and §10.7; pinned Pi basis:
/// `packages/ai/test/fireworks-models.test.ts`.
#[test]
fn fireworks_models_catalog_and_compat_contract_pi_exact() {
    let kimi = pinned_model("fireworks", "accounts/fireworks/models/kimi-k2p6");
    assert_eq!(
        kimi.common.base_url.as_str(),
        "https://api.fireworks.ai/inference"
    );
    assert!(kimi.common.reasoning);
    assert_eq!(
        kimi.common.modalities.input,
        BTreeSet::from([Modality::Text, Modality::Image])
    );
    assert_eq!(kimi.common.limits.context_window, 262_000);
    assert_eq!(kimi.common.limits.max_output_tokens, 262_000);
    assert_eq!(kimi.common.pricing.default.input, MoneyRate::new(950_000));
    assert_eq!(
        kimi.common.pricing.default.output,
        MoneyRate::new(4_000_000)
    );
    assert_eq!(
        kimi.common.pricing.default.cache_read,
        MoneyRate::new(160_000)
    );
    let ApiModelConfig::AnthropicMessages(kimi_config) = kimi.api else {
        panic!("Fireworks Kimi K2.6 must use Anthropic Messages")
    };
    assert_eq!(kimi_config.compat.send_session_affinity_headers, Some(true));
    assert_eq!(
        kimi_config.compat.supports_eager_tool_input_streaming,
        Some(false)
    );
    assert_eq!(
        kimi_config.compat.supports_cache_control_on_tools,
        Some(false)
    );
    assert_eq!(
        kimi_config.compat.supports_long_cache_retention,
        Some(false)
    );

    let models = remaining_provider_models("fireworks").expect("Fireworks catalog");
    assert!(models.iter().any(|model| {
        model
            .common
            .model_ref
            .model
            .as_str()
            .starts_with("accounts/fireworks/routers/")
            && model.common.model_ref.model.as_str().ends_with("-turbo")
            && model.api.api_id() == ApiId::new("anthropic-messages")
            && model.common.modalities.input.contains(&Modality::Image)
    }));

    let glm = pinned_model("fireworks", "accounts/fireworks/models/glm-5p2");
    let glm_fast = pinned_model("fireworks", "accounts/fireworks/routers/glm-5p2-fast");
    assert_eq!(glm.api, glm_fast.api);

    let kimi_k3 = pinned_model("fireworks", "accounts/fireworks/models/kimi-k3");
    let kimi_k3_fast = pinned_model("fireworks", "accounts/fireworks/routers/kimi-k3-fast");
    assert_eq!(kimi_k3.api, kimi_k3_fast.api);
    assert_eq!(
        kimi_k3.common.base_url.as_str(),
        "https://api.fireworks.ai/inference/v1"
    );
    let ApiModelConfig::OpenAiCompletions(kimi_k3_config) = kimi_k3.api else {
        panic!("Fireworks Kimi K3 must use OpenAI Completions")
    };
    assert_eq!(kimi_k3_config.compat.supports_store, Some(false));
    assert_eq!(kimi_k3_config.compat.supports_developer_role, Some(false));
    assert_eq!(
        kimi_k3_config
            .compat
            .requires_reasoning_content_on_assistant_messages,
        Some(true)
    );
    assert_eq!(
        kimi_k3_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::OpenAi)
    );
    assert_eq!(
        kimi_k3_config.compat.deferred_tools_mode,
        Some(DeferredToolsMode::Kimi)
    );
    assert_eq!(
        kimi_k3_config.compat.send_session_affinity_headers,
        Some(true)
    );
    assert_eq!(
        kimi_k3_config.compat.supports_long_cache_retention,
        Some(false)
    );
    assert_eq!(kimi_k3_config.thinking_levels.off, unsupported());
    assert_eq!(kimi_k3_config.thinking_levels.low, effort("low"));
    assert_eq!(kimi_k3_config.thinking_levels.medium, effort("medium"));
    assert_eq!(kimi_k3_config.thinking_levels.high, effort("high"));
    assert_eq!(kimi_k3_config.thinking_levels.max, effort("max"));
}

/// Architecture v2 part 2 §5.1, §5.2, and §10.7; pinned Pi basis:
/// `packages/ai/test/qwen-token-plan-models.test.ts`.
#[test]
fn qwen_token_plan_catalog_and_reasoning_contract_pi_exact() {
    let expected_text = BTreeSet::from([
        "MiniMax-M2.5",
        "deepseek-v3.2",
        "deepseek-v4-flash",
        "deepseek-v4-pro",
        "glm-5",
        "glm-5.1",
        "glm-5.2",
        "kimi-k2.5",
        "kimi-k2.6",
        "kimi-k2.7-code",
        "qwen3.6-flash",
        "qwen3.6-plus",
        "qwen3.7-max",
        "qwen3.7-plus",
        "qwen3.8-max",
    ]);
    let excluded_images = [
        "qwen-image-2.0",
        "qwen-image-2.0-pro",
        "wan2.7-image",
        "wan2.7-image-pro",
    ];
    for provider in ["qwen-token-plan", "qwen-token-plan-cn"] {
        let models = remaining_provider_models(provider).expect("Qwen catalog");
        let ids = models
            .iter()
            .map(|model| model.common.model_ref.model.as_str())
            .collect::<BTreeSet<_>>();
        assert!(expected_text.is_subset(&ids), "{provider}");
        for excluded in excluded_images {
            assert!(!ids.contains(excluded), "{provider}/{excluded}");
        }
    }

    let individual =
        remaining_provider_models("qwen-token-plan-individual").expect("Qwen Individual catalog");
    assert_eq!(
        individual
            .iter()
            .map(|model| model.common.model_ref.model.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "deepseek-v4-flash-0731",
            "deepseek-v4-pro",
            "deepseek-v4-pro-0813",
            "glm-5.2",
            "qwen3.6-flash",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.8-max",
        ])
    );

    for provider in [
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
    ] {
        assert!(
            remaining_provider_models(provider)
                .expect("Qwen catalog")
                .iter()
                .all(|model| model.common.model_ref.model.as_str() != "qwen3.8-max-preview")
        );
        let qwen = pinned_model(provider, "qwen3.8-max");
        let ApiModelConfig::OpenAiCompletions(config) = qwen.api else {
            panic!("Qwen 3.8 Max must use OpenAI Completions")
        };
        assert_eq!(
            config.compat.thinking_format,
            Some(OpenAiThinkingFormat::Qwen)
        );
        assert_eq!(config.compat.supports_reasoning_effort, Some(true));
        assert_eq!(config.thinking_levels.low, effort("low"));
        assert_eq!(config.thinking_levels.medium, effort("medium"));
        assert_eq!(config.thinking_levels.xhigh, effort("xhigh"));
        assert_eq!(config.thinking_levels.high, unsupported());
    }

    for (provider, ids) in [
        (
            "qwen-token-plan",
            &[
                "deepseek-v4-flash",
                "deepseek-v4-pro",
                "glm-5",
                "glm-5.1",
                "glm-5.2",
            ][..],
        ),
        (
            "qwen-token-plan-cn",
            &[
                "deepseek-v4-flash",
                "deepseek-v4-pro",
                "glm-5",
                "glm-5.1",
                "glm-5.2",
            ][..],
        ),
        (
            "qwen-token-plan-individual",
            &[
                "deepseek-v4-flash-0731",
                "deepseek-v4-pro",
                "deepseek-v4-pro-0813",
                "glm-5.2",
            ][..],
        ),
    ] {
        for id in ids {
            let model = pinned_model(provider, id);
            let ApiModelConfig::OpenAiCompletions(config) = model.api else {
                panic!("Qwen effort model must use OpenAI Completions")
            };
            assert_eq!(
                config.compat.thinking_format,
                Some(OpenAiThinkingFormat::Qwen)
            );
            assert_eq!(config.compat.supports_reasoning_effort, Some(true));
            assert_eq!(config.thinking_levels.high, effort("high"));
            assert_eq!(config.thinking_levels.max, effort("max"));
        }
    }
}

/// Architecture v2 part 2 §5.1 and §10.7; pinned Pi basis:
/// `packages/ai/test/xiaomi-models.test.ts`.
#[test]
fn xiaomi_catalog_replacement_ids_pi_exact() {
    for provider in [
        "xiaomi",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-sgp",
    ] {
        let ids = remaining_provider_models(provider)
            .expect("Xiaomi catalog")
            .into_iter()
            .map(|model| model.common.model_ref.model.to_string())
            .collect::<BTreeSet<_>>();
        for retired in ["mimo-v2-flash", "mimo-v2-omni", "mimo-v2-pro"] {
            assert!(!ids.contains(retired), "{provider}/{retired}");
        }
        for replacement in ["mimo-v2.5", "mimo-v2.5-pro"] {
            assert!(ids.contains(replacement), "{provider}/{replacement}");
        }
    }
}

/// Architecture v2 part 2 §5.3, §5.4, and §10.7; pinned Pi basis:
/// `packages/ai/test/generate-models-strict.test.ts`.
#[test]
fn qwen_individual_strict_generation_validates_before_publication_pi_exact() {
    use agentprism_qwen_token_plan_individual::{
        STRICT_MODEL_IDS, StrictSourceModel, validate_strict_source_models,
    };

    let published_before = remaining_provider_models("qwen-token-plan-individual")
        .expect("published Individual catalog");
    let candidates = STRICT_MODEL_IDS
        .iter()
        .map(|id| StrictSourceModel {
            id,
            supports_tools: *id != "deepseek-v4-flash-0731",
        })
        .collect::<Vec<_>>();
    let error = validate_strict_source_models(candidates)
        .expect_err("loss of tool support invalidates the complete candidate");
    assert_eq!(
        error.to_string(),
        "qwen-token-plan-individual model IDs do not match (missing: deepseek-v4-flash-0731)"
    );
    assert_eq!(error.missing(), ["deepseek-v4-flash-0731"]);
    assert!(error.extra().is_empty());
    assert_eq!(
        remaining_provider_models("qwen-token-plan-individual")
            .expect("published catalog remains available"),
        published_before
    );

    let valid =
        validate_strict_source_models(STRICT_MODEL_IDS.iter().map(|id| StrictSourceModel {
            id,
            supports_tools: true,
        }))
        .expect("complete tool-capable candidate validates");
    assert_eq!(
        valid.into_iter().collect::<BTreeSet<_>>(),
        STRICT_MODEL_IDS.iter().copied().collect()
    );
}

/// Architecture v2 part 2 §5.1, §5.2, and §10.7; pinned Pi basis:
/// `packages/ai/test/zai-coding-plan-models.test.ts`,
/// "uses API-equivalent reference costs for Coding Plan models".
#[test]
fn zai_coding_plan_catalog_costs_match_new_pin_pi_exact() {
    let vision = pinned_model("zai-coding-cn", "glm-4.6v");
    assert_eq!(vision.common.display_name, "GLM-4.6V");
    assert_eq!(
        vision.common.base_url.as_str(),
        "https://open.bigmodel.cn/api/coding/paas/v4"
    );
    assert!(vision.common.reasoning);
    assert_eq!(
        vision.common.modalities.input,
        BTreeSet::from([Modality::Text, Modality::Image])
    );
    assert_eq!(vision.common.limits.context_window, 128_000);
    assert_eq!(vision.common.limits.max_output_tokens, 32_768);
    assert_eq!(vision.common.pricing.default.input, MoneyRate::new(300_000));
    assert_eq!(
        vision.common.pricing.default.output,
        MoneyRate::new(900_000)
    );
    assert_eq!(vision.common.pricing.default.cache_read, MoneyRate::new(0));
    assert_eq!(vision.common.pricing.default.cache_write, MoneyRate::new(0));
    let ApiModelConfig::OpenAiCompletions(vision_config) = vision.api else {
        panic!("Z.AI GLM-4.6V must use OpenAI Completions")
    };
    assert_eq!(
        vision_config.compat.max_tokens_field,
        Some(MaxTokensField::MaxTokens)
    );
    assert_eq!(
        vision_config.compat.thinking_format,
        Some(OpenAiThinkingFormat::Zai)
    );
    assert_eq!(vision_config.compat.zai_tool_stream, Some(true));

    for (provider, model_id, input, output, cache_read) in [
        ("zai", "glm-5.2", 1_400_000, 4_400_000, 260_000),
        ("zai-coding-cn", "glm-5.1", 1_400_000, 4_400_000, 260_000),
        (
            "zai-coding-cn",
            "glm-5v-turbo",
            1_200_000,
            4_000_000,
            240_000,
        ),
    ] {
        let model = pinned_model(provider, model_id);
        assert_eq!(model.common.pricing.default.input, MoneyRate::new(input));
        assert_eq!(model.common.pricing.default.output, MoneyRate::new(output));
        assert_eq!(
            model.common.pricing.default.cache_read,
            MoneyRate::new(cache_read)
        );
        assert_eq!(model.common.pricing.default.cache_write, MoneyRate::new(0));
    }

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

        let highspeed = pinned_model(provider, "glm-5.2-highspeed");
        assert_eq!(highspeed.common.pricing.default.input, MoneyRate::new(0));
        assert_eq!(highspeed.common.pricing.default.output, MoneyRate::new(0));
        assert_eq!(
            highspeed.common.pricing.default.cache_read,
            MoneyRate::new(0)
        );
        assert_eq!(
            highspeed.common.pricing.default.cache_write,
            MoneyRate::new(0)
        );
    }
}

/// Architecture v2 part 2 §5.2 and §10.7; pinned Pi basis:
/// `packages/ai/test/zen.test.ts`. The upstream suite is credential-gated and
/// reaches live services; the hermetic equivalent proves that every pinned
/// OpenCode model has a registered API-family execution capability.
#[test]
fn opencode_catalog_models_have_stream_capabilities_pi_exact() {
    for registration in [
        agentprism_opencode::provider(agentprism_provider_common::ProviderInputs {
            http: Arc::new(NoNetwork),
            environment: BTreeMap::new(),
        })
        .expect("OpenCode Zen registration"),
        agentprism_opencode_go::provider(agentprism_provider_common::ProviderInputs {
            http: Arc::new(NoNetwork),
            environment: BTreeMap::new(),
        })
        .expect("OpenCode Go registration"),
    ] {
        let models = registration.catalog.snapshot();
        assert!(!models.is_empty());
        for model in models.iter() {
            assert_eq!(model.common.model_ref.provider, registration.descriptor.id);
            assert!(
                registration.apis.contains_key(&model.api.api_id()),
                "{}/{} has no {} execution capability",
                model.common.model_ref.provider,
                model.common.model_ref.model,
                model.api.api_id()
            );
        }
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
fn anthropic_messages_e2e_catalog_coverage_send_and_local_pi_exact() {
    // Architecture v2 part 2 §10.7. Pi basis:
    // packages/ai/test/anthropic-eager-tool-input-e2e.test.ts and
    // anthropic-long-cache-retention-e2e.test.ts `covers every generated`
    // scenarios. Catalog enumeration is hermetic; request acceptance is
    // covered by the Anthropic API-family request tests.
    let send = builtin_providers(BuiltinProviderInputs {
        http: Arc::new(NoNetwork),
        bedrock: Arc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .expect("complete Send provider catalog");
    let local = local_builtin_providers(LocalBuiltinProviderInputs {
        http: Rc::new(NoNetwork),
        bedrock: Rc::new(NoNetwork),
        environment: BTreeMap::new(),
    })
    .expect("complete local provider catalog");

    let anthropic_models = |providers: &[ProviderRegistration]| {
        providers
            .iter()
            .flat_map(|provider| provider.catalog.snapshot().to_vec())
            .filter(|model| model.api.api_id() == ApiId::new("anthropic-messages"))
            .map(|model| {
                assert!(matches!(model.api, ApiModelConfig::AnthropicMessages(_)));
                (
                    model.common.model_ref.provider.to_string(),
                    model.common.model_ref.model.to_string(),
                )
            })
            .collect::<BTreeSet<_>>()
    };
    let local_anthropic_models = |providers: &[LocalProviderRegistration]| {
        providers
            .iter()
            .flat_map(|provider| provider.catalog.snapshot().to_vec())
            .filter(|model| model.api.api_id() == ApiId::new("anthropic-messages"))
            .map(|model| {
                assert!(matches!(model.api, ApiModelConfig::AnthropicMessages(_)));
                (
                    model.common.model_ref.provider.to_string(),
                    model.common.model_ref.model.to_string(),
                )
            })
            .collect::<BTreeSet<_>>()
    };

    let send_models = anthropic_models(&send);
    assert_eq!(send_models, local_anthropic_models(&local));
    assert!(!send_models.is_empty());
    assert_eq!(
        send_models
            .iter()
            .map(|(provider, _)| provider.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "anthropic",
            "cloudflare-ai-gateway",
            "fireworks",
            "github-copilot",
            "kimi-coding",
            "minimax",
            "minimax-cn",
            "opencode",
            "opencode-go",
            "vercel-ai-gateway",
        ])
    );
}

#[test]
fn openrouter_anthropic_latest_cache_control_catalog_pi_exact() {
    // Architecture v2 part 2 §10.7. Pi basis:
    // packages/ai/test/openrouter-cache-control-models.test.ts.
    let models = agentprism_openrouter::models().expect("OpenRouter catalog");
    for id in [
        "~anthropic/claude-fable-latest",
        "~anthropic/claude-haiku-latest",
        "~anthropic/claude-opus-latest",
        "~anthropic/claude-sonnet-latest",
    ] {
        let model = models
            .iter()
            .find(|model| model.common.model_ref.model.as_str() == id)
            .unwrap_or_else(|| panic!("missing OpenRouter model {id}"));
        let ApiModelConfig::OpenAiCompletions(config) = &model.api else {
            panic!("OpenRouter {id} must use OpenAI Completions")
        };
        assert_eq!(
            config.compat.cache_control_format,
            Some(CacheControlFormat::Anthropic),
            "{id}"
        );
    }
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
