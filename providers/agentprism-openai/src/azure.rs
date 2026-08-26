//! Azure OpenAI Responses lowering over the shared Responses contracts.

use agentprism_ai::{
    ApiFamily, CustomApiModelConfig, EncodeContext, EncodeError, OpenAiResponses,
    OpenAiResponsesCompat, OpenAiResponsesModelConfig, OpenAiResponsesOptions,
    OpenAiResponsesSimplePatch, OrderedJsonObject, OrderedJsonString, SimpleGenerationOptions,
    SimpleLoweringContext, TypedModelDescriptor, trim_ecmascript,
};
use serde::{Deserialize, Serialize};
use url::Url;

/// Azure OpenAI Responses API-family marker.
#[derive(Clone, Copy, Debug, Default)]
pub struct AzureOpenAiResponses;

/// Typed custom configuration stored under architecture §5.1's open API seam.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AzureOpenAiResponsesModelConfig {
    /// Shared Responses compatibility and reasoning maps.
    pub responses: OpenAiResponsesModelConfig,
}

/// Fully lowered Azure Responses options.
#[derive(Clone, Debug, PartialEq)]
pub struct AzureOpenAiResponsesOptions {
    /// Shared Responses options.
    pub responses: OpenAiResponsesOptions,
    /// Explicit Azure service base URL, projected before auth resolution.
    pub azure_base_url: Option<Url>,
    /// Explicit Azure resource name used when no base URL is supplied.
    pub azure_resource_name: Option<String>,
    /// Azure deployment name placed in the request's `model` property.
    pub deployment_name: String,
    /// Azure REST API version used by the transport.
    pub api_version: String,
}

/// Azure-specific simple-options patch.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AzureOpenAiResponsesSimplePatch {
    /// Shared reasoning-summary override.
    pub reasoning_summary: Option<agentprism_ai::OpenAiResponsesReasoningSummary>,
    /// Explicit Azure service base URL.
    pub azure_base_url: Option<Url>,
    /// Explicit Azure resource name.
    pub azure_resource_name: Option<String>,
    /// Explicit deployment override.
    pub azure_deployment_name: Option<String>,
    /// Explicit Azure API version; pinned Pi defaults to `v1`.
    pub azure_api_version: Option<String>,
}

impl ApiFamily for AzureOpenAiResponses {
    const API_ID: &'static str = "azure-openai-responses";

    type Compat = OpenAiResponsesCompat;
    type ModelConfig = CustomApiModelConfig;
    type FullOptions = AzureOpenAiResponsesOptions;
    type OptionsPatch = AzureOpenAiResponsesSimplePatch;
    type WireRequest = OrderedJsonObject;

    fn resolve_compat(
        effective_base_url: &Url,
        model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, agentprism_ai::LoweringError> {
        OpenAiResponses::resolve_compat(effective_base_url, model_overrides)
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, agentprism_ai::LoweringError> {
        let config = azure_model_config(&context.model.config)
            .map_err(|message| agentprism_ai::LoweringError::InvalidConfiguration { message })?;
        let shared = TypedModelDescriptor::<OpenAiResponses> {
            common: context.model.common.clone(),
            config: config.responses,
            extensions: context.model.extensions.clone(),
        };
        let responses = OpenAiResponses::lower_simple(
            SimpleLoweringContext {
                model: &shared,
                compat: context.compat,
                effective_base_url: context.effective_base_url,
                estimated_input_tokens: context.estimated_input_tokens,
                available_context_tokens: context.available_context_tokens,
            },
            simple,
            &OpenAiResponsesSimplePatch {
                reasoning_summary: patch.reasoning_summary,
                service_tier: None,
            },
        )?;
        Ok(AzureOpenAiResponsesOptions {
            responses,
            azure_base_url: patch.azure_base_url.clone(),
            azure_resource_name: patch.azure_resource_name.clone(),
            deployment_name: patch
                .azure_deployment_name
                .clone()
                .filter(|deployment| !deployment.is_empty())
                .unwrap_or_else(|| context.model.common.model_ref.model.to_string()),
            api_version: patch
                .azure_api_version
                .clone()
                .filter(|version| !version.is_empty())
                .unwrap_or_else(|| "v1".into()),
        })
    }

    fn encode(
        context: EncodeContext<'_, Self>,
        options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        let config = azure_model_config(&context.model.config)
            .map_err(|message| EncodeError::InvalidRequest { message })?;
        let shared = TypedModelDescriptor::<OpenAiResponses> {
            common: context.model.common.clone(),
            config: config.responses,
            extensions: context.model.extensions.clone(),
        };
        // The shared Responses encoder keys replay applicability by its own
        // API marker. Adapt only the provider-view envelope so Azure artifacts
        // captured under `azure-openai-responses` remain exact on turn two;
        // canonical persisted history is never mutated.
        let mut projected = context.context.clone();
        for message in &mut projected.messages {
            if let agentprism_ai::Message::Assistant(assistant) = message
                && assistant.replay.source.api.as_str() == Self::API_ID
            {
                assistant.replay.source.api = agentprism_ai::ApiId::new(OpenAiResponses::API_ID);
                assistant.api = agentprism_ai::ApiId::new(OpenAiResponses::API_ID);
            }
        }
        let mut request = OpenAiResponses::encode(
            EncodeContext {
                model: &shared,
                context: &projected,
                compat: context.compat,
                effective_base_url: context.effective_base_url,
            },
            &options.responses,
        )?;
        request.insert("model", options.deployment_name.clone());
        request.remove("prompt_cache_key");
        request.remove("prompt_cache_retention");
        request.remove("prompt_cache_options");
        request.remove("service_tier");
        let prompt_cache_key =
            agentprism_ai::clamp_openai_prompt_cache_key(options.responses.session_id.as_deref());
        let mut azure_request = OrderedJsonObject::new();
        for (name, value) in request {
            let is_stream = name == OrderedJsonString::from("stream");
            azure_request.insert(name, value);
            if is_stream && let Some(key) = prompt_cache_key.as_deref() {
                azure_request.insert("prompt_cache_key", key);
            }
        }
        Ok(azure_request)
    }
}

/// Parses Pi's comma-separated `model=deployment` environment value.
pub fn parse_azure_deployment_name_map(value: &str) -> Vec<(String, String)> {
    let mut map = Vec::<(String, String)>::new();
    for entry in value.split(',') {
        let trimmed = trim_ecmascript(entry);
        if trimmed.is_empty() {
            continue;
        }
        let mut components = trimmed.split('=');
        let Some(model) = components.next() else {
            continue;
        };
        let Some(deployment) = components.next() else {
            continue;
        };
        if model.is_empty() || deployment.is_empty() {
            continue;
        }
        let model = trim_ecmascript(model).to_owned();
        let deployment = trim_ecmascript(deployment).to_owned();
        if let Some((_, current)) = map.iter_mut().find(|(current, _)| current == &model) {
            *current = deployment;
        } else {
            map.push((model, deployment));
        }
    }
    map
}

/// Decodes typed Azure custom model configuration.
pub fn azure_model_config(
    config: &CustomApiModelConfig,
) -> Result<AzureOpenAiResponsesModelConfig, String> {
    if config.api.as_str() != AzureOpenAiResponses::API_ID {
        return Err(format!(
            "custom model uses {}, not azure-openai-responses",
            config.api
        ));
    }
    if config.schema_version != 1 {
        return Err(format!(
            "unsupported Azure model schema version {}",
            config.schema_version
        ));
    }
    serde_json::from_str(config.value.get())
        .map_err(|error| format!("invalid Azure model configuration: {error}"))
}

/// Normalizes Azure service roots to the `/openai/v1` SDK base while
/// preserving explicit non-Azure proxy paths and query parameters.
pub fn normalize_azure_openai_base_url(value: &str) -> Result<Url, url::ParseError> {
    let mut url = Url::parse(trim_ecmascript(value).trim_end_matches('/'))?;
    let host = url.host_str().unwrap_or_default();
    let azure = host.ends_with(".openai.azure.com")
        || host.ends_with(".cognitiveservices.azure.com")
        || host.ends_with(".ai.azure.com");
    let path = url.path().trim_end_matches('/');
    if azure && matches!(path, "" | "/" | "/openai" | "/openai/v1/responses") {
        url.set_path("/openai/v1");
        url.set_query(None);
    }
    Ok(url)
}
