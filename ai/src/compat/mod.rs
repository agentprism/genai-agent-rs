pub mod extension_oauth_types;

use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::providers::all::{
    builtin_models, get_builtin_model, get_builtin_models, get_builtin_providers,
};
use indexmap::IndexMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

pub use crate::api::anthropic_messages::anthropic_messages_api;
pub use crate::api::bedrock_converse_stream::bedrock_converse_stream_api;
pub use crate::api::google_generative_ai::google_generative_ai_api;
pub use crate::api::google_vertex::google_vertex_api;
pub use crate::api::openai_codex_responses::open_ai_codex_responses_api;
pub use crate::api::openai_completions::open_ai_completions_api;
pub use crate::api::openai_responses::open_ai_responses_api;
pub use crate::auth::{context::*, credential_store::*, helpers::*, types::*};
pub use crate::env_api_keys::*;
pub use crate::event_stream::*;
pub use crate::legacy_api_aliases::*;
pub use crate::models::*;
pub use crate::models_store::*;
pub use crate::providers::all::BuiltinProvider;
pub use crate::providers::faux::*;
pub use crate::session_resources::*;
pub use crate::types::*;
pub use crate::utils::diagnostics::*;
pub use crate::utils::json_parse::*;
pub use crate::utils::overflow::*;
pub use crate::utils::retry::*;
pub use crate::utils::text::content_text;
pub use crate::utils::typebox_helpers::*;
pub use crate::utils::uuid::uuid_v7;
pub use crate::utils::validation::*;
pub use crate::{
    AnthropicEffort, AnthropicOptions, AnthropicThinkingDisplay, BedrockOptions,
    BedrockThinkingDisplay, GoogleApiThinkingLevel, GoogleOptions, GoogleVertexOptions,
    OpenAICodexResponsesOptions, OpenAICodexWebSocketDebugStats, OpenAICompletionsOptions,
    OpenAIResponsesOptions, ResolvedGoogleThinkingLevel,
};
pub use crate::{auth, models, models_store, providers, session_resources, types, utils};
pub use extension_oauth_types::*;

const AMBIENT_AUTH_MARKER: &str = "<authenticated>";

pub type ApiStreamFunction = Arc<
    dyn Fn(&Model, &Context, Option<StreamOptions>) -> AssistantMessageEventStream + Send + Sync,
>;
pub type ApiStreamSimpleFunction = Arc<
    dyn Fn(&Model, &Context, Option<SimpleStreamOptions>) -> AssistantMessageEventStream
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ApiProvider {
    pub api: Api,
    pub streams: Arc<dyn ProviderStreams>,
}

impl ApiProvider {
    pub fn new(api: impl Into<Api>, streams: Arc<dyn ProviderStreams>) -> Self {
        Self {
            api: api.into(),
            streams,
        }
    }
}

#[derive(Clone)]
struct RegisteredApiProvider {
    provider: ApiProvider,
    source_id: Option<String>,
}

struct CheckedStreams {
    api: Api,
    inner: Arc<dyn ProviderStreams>,
}

impl ProviderStreams for CheckedStreams {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        if model.api != self.api {
            panic!("Mismatched api: {} expected {}", model.api, self.api);
        }
        self.inner.stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        if model.api != self.api {
            panic!("Mismatched api: {} expected {}", model.api, self.api);
        }
        self.inner.stream_simple(model, context, options)
    }

    fn deferred(&self) -> Option<&dyn crate::api::DeferredStreams> {
        self.inner.deferred()
    }
}

#[derive(Default)]
struct ApiRegistry {
    entries: IndexMap<String, RegisteredApiProvider>,
    builtin_instances: IndexMap<String, Arc<dyn ProviderStreams>>,
}

impl ApiRegistry {
    fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register_builtins();
        registry
    }

    fn register(
        &mut self,
        provider: ApiProvider,
        source_id: Option<String>,
    ) -> Arc<dyn ProviderStreams> {
        let api = provider.api.clone();
        let streams: Arc<dyn ProviderStreams> = Arc::new(CheckedStreams {
            api: api.clone(),
            inner: provider.streams,
        });
        self.entries.insert(
            api.to_string(),
            RegisteredApiProvider {
                provider: ApiProvider {
                    api,
                    streams: streams.clone(),
                },
                source_id,
            },
        );
        streams
    }

    fn register_builtins(&mut self) {
        for provider in builtin_api_providers() {
            let key = provider.api.to_string();
            if !self.entries.contains_key(&key) {
                self.register(provider, None);
            }
            if let Some(entry) = self.entries.get(&key) {
                self.builtin_instances
                    .insert(key, entry.provider.streams.clone());
            }
        }
    }
}

fn builtin_api_providers() -> Vec<ApiProvider> {
    vec![
        ApiProvider::new("anthropic-messages", Arc::new(anthropic_messages_api())),
        ApiProvider::new("openai-completions", Arc::new(open_ai_completions_api())),
        ApiProvider::new("openai-responses", Arc::new(open_ai_responses_api())),
        ApiProvider::new(
            "openai-codex-responses",
            Arc::new(open_ai_codex_responses_api()),
        ),
        ApiProvider::new("google-generative-ai", Arc::new(google_generative_ai_api())),
        ApiProvider::new("google-vertex", Arc::new(google_vertex_api())),
        ApiProvider::new(
            "bedrock-converse-stream",
            Arc::new(bedrock_converse_stream_api()),
        ),
    ]
}

static API_REGISTRY: LazyLock<RwLock<ApiRegistry>> =
    LazyLock::new(|| RwLock::new(ApiRegistry::with_builtins()));
static COMPAT_MODELS: LazyLock<Models> = LazyLock::new(|| builtin_models(None));
static FAUX_SOURCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn register_api_provider(provider: ApiProvider, source_id: Option<String>) {
    API_REGISTRY
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .register(provider, source_id);
}

pub fn get_api_provider(api: &Api) -> Option<ApiProvider> {
    API_REGISTRY
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .entries
        .get(api.as_str())
        .map(|entry| entry.provider.clone())
}

pub fn get_api_providers() -> Vec<ApiProvider> {
    API_REGISTRY
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .entries
        .values()
        .map(|entry| entry.provider.clone())
        .collect()
}

pub fn unregister_api_providers(source_id: &str) {
    API_REGISTRY
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .entries
        .retain(|_, entry| entry.source_id.as_deref() != Some(source_id));
}

pub fn register_built_in_api_providers() {
    API_REGISTRY
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .register_builtins();
}

pub fn reset_api_providers() {
    *API_REGISTRY.write().unwrap_or_else(PoisonError::into_inner) = ApiRegistry::with_builtins();
}

pub struct FauxProviderRegistration {
    pub api: String,
    pub models: Vec<Model>,
    pub state: Arc<FauxProviderState>,
    source_id: String,
    core: FauxCore,
}

impl FauxProviderRegistration {
    pub fn get_model(&self, model_id: Option<&str>) -> Option<Model> {
        self.core.get_model(model_id)
    }

    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        self.core.set_responses(responses);
    }

    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.core.append_responses(responses);
    }

    pub fn pending_response_count(&self) -> usize {
        self.core.pending_response_count()
    }

    pub fn unregister(&self) {
        unregister_api_providers(&self.source_id);
    }
}

pub fn register_faux_provider(options: RegisterFauxProviderOptions) -> FauxProviderRegistration {
    let core = create_faux_core(options);
    let source_id = format!(
        "faux-provider-{}",
        FAUX_SOURCE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    register_api_provider(
        ApiProvider::new(core.api(), Arc::new(core.clone())),
        Some(source_id.clone()),
    );
    FauxProviderRegistration {
        api: core.api().to_owned(),
        models: core.models().to_vec(),
        state: core.state(),
        source_id,
        core,
    }
}

pub fn get_model(provider: &str, model_id: &str) -> Option<&'static Model> {
    get_builtin_model(provider, model_id)
}

pub fn get_models(provider: &str) -> Vec<Model> {
    get_builtin_models(provider)
}

pub fn get_providers() -> Vec<&'static str> {
    get_builtin_providers()
}

fn request_options_mut(options: &mut ApiStreamOptions) -> &mut ProviderRequestOptions<Model> {
    match options {
        ApiStreamOptions::Base(options) => &mut options.request,
        ApiStreamOptions::AnthropicMessages(options) => &mut options.stream.request,
        ApiStreamOptions::BedrockConverseStream(options) => &mut options.stream.request,
        ApiStreamOptions::GoogleGenerativeAI(options) => &mut options.stream.request,
        ApiStreamOptions::GoogleVertex(options) => &mut options.stream.request,
        ApiStreamOptions::OpenAICompletions(options) => &mut options.stream.request,
        ApiStreamOptions::OpenAIResponses(options) => &mut options.stream.request,
        ApiStreamOptions::OpenAICodexResponses(options) => &mut options.stream.request,
        ApiStreamOptions::Custom { base, .. } => &mut base.request,
    }
}

fn has_explicit_api_key(value: Option<&String>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn with_env_api_key(model: &Model, mut options: ApiStreamOptions) -> ApiStreamOptions {
    let request = request_options_mut(&mut options);
    if has_explicit_api_key(request.api_key.as_ref()) {
        return options;
    }
    if let Some(api_key) =
        crate::env_api_keys::get_env_api_key(model.provider.as_str(), request.env.as_ref())
            .filter(|api_key| api_key != AMBIENT_AUTH_MARKER)
    {
        request.api_key = Some(api_key);
    }
    options
}

fn with_simple_env_api_key(model: &Model, mut options: SimpleStreamOptions) -> SimpleStreamOptions {
    if has_explicit_api_key(options.stream.request.api_key.as_ref()) {
        return options;
    }
    if let Some(api_key) = crate::env_api_keys::get_env_api_key(
        model.provider.as_str(),
        options.stream.request.env.as_ref(),
    )
    .filter(|api_key| api_key != AMBIENT_AUTH_MARKER)
    {
        options.stream.request.api_key = Some(api_key);
    }
    options
}

fn has_resolved_cloudflare_auth(request: &ProviderRequestOptions<Model>) -> bool {
    has_explicit_api_key(request.api_key.as_ref())
        || request.headers.as_ref().is_some_and(|headers| {
            headers
                .get("cf-aig-authorization")
                .is_some_and(Option::is_some)
        })
}

fn get_builtin_provider_for_model(model: &Model) -> Option<ProviderRef> {
    let registry = API_REGISTRY.read().unwrap_or_else(PoisonError::into_inner);
    let current = registry.entries.get(model.api.as_str())?;
    let builtin = registry.builtin_instances.get(model.api.as_str())?;
    if !Arc::ptr_eq(&current.provider.streams, builtin) {
        return None;
    }
    drop(registry);
    let provider = COMPAT_MODELS.get_provider(model.provider.as_str())?;
    provider
        .get_models()
        .ok()?
        .iter()
        .any(|candidate| candidate.api == model.api)
        .then_some(provider)
}

pub fn stream(
    model: &Model,
    context: &Context,
    options: Option<ApiStreamOptions>,
) -> AssistantMessageEventStream {
    let options = options.unwrap_or_else(|| ApiStreamOptions::Base(Default::default()));
    if let Some(provider) = get_builtin_provider_for_model(model) {
        let request = match &options {
            ApiStreamOptions::Base(options) => &options.request,
            ApiStreamOptions::AnthropicMessages(options) => &options.stream.request,
            ApiStreamOptions::BedrockConverseStream(options) => &options.stream.request,
            ApiStreamOptions::GoogleGenerativeAI(options) => &options.stream.request,
            ApiStreamOptions::GoogleVertex(options) => &options.stream.request,
            ApiStreamOptions::OpenAICompletions(options) => &options.stream.request,
            ApiStreamOptions::OpenAIResponses(options) => &options.stream.request,
            ApiStreamOptions::OpenAICodexResponses(options) => &options.stream.request,
            ApiStreamOptions::Custom { base, .. } => &base.request,
        };
        if model.provider.as_str().starts_with("cloudflare-")
            && !has_resolved_cloudflare_auth(request)
        {
            return COMPAT_MODELS.stream(
                model,
                context,
                ModelsApiStreamOptions {
                    options,
                    transform_headers: None,
                },
            );
        }
        return provider.stream(model, context, with_env_api_key(model, options));
    }
    let Some(provider) = get_api_provider(&model.api) else {
        panic!("No API provider registered for api: {}", model.api);
    };
    provider
        .streams
        .stream(model, context, with_env_api_key(model, options))
}

pub async fn complete(
    model: &Model,
    context: &Context,
    options: Option<ApiStreamOptions>,
) -> Result<AssistantMessage, StreamProtocolError> {
    stream(model, context, options).result().await
}

pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: Option<SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let options = options.unwrap_or_default();
    if let Some(provider) = get_builtin_provider_for_model(model) {
        if model.provider.as_str().starts_with("cloudflare-")
            && !has_resolved_cloudflare_auth(&options.stream.request)
        {
            return COMPAT_MODELS.stream_simple(
                model,
                context,
                ModelsSimpleStreamOptions {
                    options,
                    transform_headers: None,
                },
            );
        }
        return provider.stream_simple(model, context, with_simple_env_api_key(model, options));
    }
    let Some(provider) = get_api_provider(&model.api) else {
        panic!("No API provider registered for api: {}", model.api);
    };
    provider
        .streams
        .stream_simple(model, context, with_simple_env_api_key(model, options))
}

pub async fn complete_simple(
    model: &Model,
    context: &Context,
    options: Option<SimpleStreamOptions>,
) -> Result<AssistantMessage, StreamProtocolError> {
    stream_simple(model, context, options).result().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::faux::{FauxAssistantMessageOptions, faux_assistant_message};
    use crate::types::{
        ModelCost, ModelCostRates, ModelInput, ProviderEnv, ProviderId, StreamOptions,
    };
    use tokio::sync::Mutex;

    static COMPAT_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn custom_model(api: &str) -> Model {
        Model {
            id: "test-model".to_owned(),
            name: "Test Model".to_owned(),
            api: api.into(),
            provider: ProviderId::from("custom-openai"),
            base_url: "https://example.test/v1".to_owned(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                rates: ModelCostRates::default(),
                tiers: None,
            },
            context_window: 128_000,
            max_tokens: 4_096,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    /// Ports pi `test/compat-env.test.ts:41-73`.
    #[tokio::test]
    async fn custom_provider_dispatch_preserves_explicit_api_key() {
        let _guard = COMPAT_TEST_LOCK.lock().await;
        reset_api_providers();
        let core = create_faux_core(RegisterFauxProviderOptions {
            api: Some("openai-responses".to_owned()),
            ..Default::default()
        });
        core.set_responses(vec![
            faux_assistant_message("ok", FauxAssistantMessageOptions::default()).into(),
        ]);
        register_api_provider(
            ApiProvider::new("openai-responses", Arc::new(core)),
            Some("test".to_owned()),
        );
        let mut options = StreamOptions::default();
        options.request.api_key = Some("request-key".to_owned());
        let result = complete(
            &custom_model("openai-responses"),
            &Context::default(),
            Some(ApiStreamOptions::Base(options)),
        )
        .await
        .expect("result");
        assert_eq!(result.stop_reason, StopReason::Stop);
        reset_api_providers();
    }

    /// Pins pi `src/compat.ts:218-229`'s explicit-key and ambient marker rules.
    #[test]
    fn env_key_injection_only_replaces_absent_or_blank_keys() {
        let _guard = COMPAT_TEST_LOCK.blocking_lock();
        let mut model = custom_model("custom-api");
        model.provider = ProviderId::from("openai");
        let env = ProviderEnv::from([("OPENAI_API_KEY".to_owned(), "ambient".to_owned())]);
        let mut base = StreamOptions::default();
        base.request.env = Some(env);
        let resolved = with_env_api_key(&model, ApiStreamOptions::Base(base));
        assert_eq!(
            request_options_mut(&mut resolved.clone())
                .api_key
                .as_deref(),
            Some("ambient")
        );

        let mut explicit = StreamOptions::default();
        explicit.request.api_key = Some("request".to_owned());
        explicit.request.env = Some(ProviderEnv::from([(
            "OPENAI_API_KEY".to_owned(),
            "ambient".to_owned(),
        )]));
        let resolved = with_env_api_key(&model, ApiStreamOptions::Base(explicit));
        assert_eq!(
            request_options_mut(&mut resolved.clone())
                .api_key
                .as_deref(),
            Some("request")
        );
    }

    /// Ports pi `src/compat.ts:126-175,242-246`.
    #[test]
    fn registry_source_unregistration_and_faux_registration_are_scoped() {
        let _guard = COMPAT_TEST_LOCK.blocking_lock();
        reset_api_providers();
        let registration = register_faux_provider(Default::default());
        let api = registration.api.clone();
        assert!(get_api_provider(&Api::from(api.clone())).is_some());
        registration.unregister();
        assert!(get_api_provider(&Api::from(api.clone())).is_none());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stream(
                &custom_model(&api),
                &Context::default(),
                None,
            )))
            .is_err()
        );
        reset_api_providers();
    }
}
