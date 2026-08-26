//! OpenAI Codex provider leaf backed by the shared Responses family.

#![deny(missing_docs)]

mod oauth;

use agentprism_ai::{
    ApiFamily, AuthError, AuthInteraction, AuthResolver, CancellationToken, LocalAuthInteraction,
    LocalAuthResolver, LocalBoxFuture, LocalOAuthAuth, LocalProviderAuthResolver,
    LocalResolveAuthRequest, OAuthAuth, ProviderAuthResolver, ResolveAuthRequest, ResolvedAuth,
    SendBoxFuture,
};
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

pub use agentprism_openai::{
    LocalOpenAiCodexResponsesTransport, LocalOpenAiCodexRetryClassifier,
    LocalOpenAiCodexWebSocketResponse, LocalOpenAiCodexWebSocketTransport,
    OpenAiCodexResponsesTransport, OpenAiCodexRetryClassifier, OpenAiCodexWebSocketConnection,
    OpenAiCodexWebSocketRequest, OpenAiCodexWebSocketResponse, OpenAiCodexWebSocketTransport,
    local_openai_codex_responses_api, local_openai_codex_responses_api_with_websocket,
    openai_codex_responses_api, openai_codex_responses_api_with_websocket,
    openai_codex_retry_policy,
};
pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pub use oauth::{
    LocalOpenAiCodexOAuth, OpenAiCodexAccessTokenAuth, OpenAiCodexOAuth, account_id_from_jwt,
};

/// Returns the pinned OpenAI Codex catalog owned by this leaf.
pub fn models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    agentprism_openai::parse_openai_published_catalog(
        include_str!("../data/models.json"),
        "openai-codex",
        "openai-codex-responses",
    )
    .map_err(ProviderBuildError::catalog)
}

/// Compatibility name for the leaf-owned OpenAI Codex catalog.
pub fn openai_codex_models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    models()
}

/// Builds the Send OpenAI Codex provider.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let api = agentprism_openai::openai_codex_responses_api(Arc::clone(&inputs.http));
    build_provider(inputs.http, api)
}

/// Builds OpenAI Codex directly from a raw Send transport.
pub fn openai_codex_provider(
    transport: Arc<dyn agentprism_ai::HttpTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    provider(ProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds the Send OpenAI Codex provider with a selectable WebSocket transport.
pub fn provider_with_websocket(
    inputs: ProviderInputs,
    websocket: Arc<dyn OpenAiCodexWebSocketTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let api = agentprism_openai::openai_codex_responses_api_with_websocket(
        Arc::clone(&inputs.http),
        websocket,
    );
    build_provider(inputs.http, api)
}

/// Builds OpenAI Codex directly with a selectable WebSocket transport.
pub fn openai_codex_provider_with_websocket(
    transport: Arc<dyn agentprism_ai::HttpTransport>,
    websocket: Arc<dyn OpenAiCodexWebSocketTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    provider_with_websocket(
        ProviderInputs {
            http: transport,
            environment: Default::default(),
        },
        websocket,
    )
}

fn build_provider(
    transport: Arc<dyn agentprism_ai::HttpTransport>,
    api: Arc<dyn agentprism_ai::ChatApi>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let oauth: Arc<dyn OAuthAuth> = Arc::new(OpenAiCodexOAuth::new(transport));
    agentprism_ai::ProviderRegistration::builder("openai-codex")
        .display_name("OpenAI Codex")
        .base_url(
            Url::parse("https://chatgpt.com/backend-api")
                .map_err(ProviderBuildError::configuration)?,
        )
        .auth(Arc::new(CodexAuthResolver::new(oauth)))
        .models(models()?)
        .api(agentprism_ai::OpenAiCodexResponses::API_ID, api)
        .retry_policy(openai_codex_retry_policy())
        .retry_classifier(Arc::new(OpenAiCodexRetryClassifier::default()))
        .build()
        .map_err(ProviderBuildError::Registration)
}

/// Builds the local OpenAI Codex provider.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let api = agentprism_openai::local_openai_codex_responses_api(Rc::clone(&inputs.http));
    build_local_provider(inputs.http, api)
}

/// Builds OpenAI Codex directly from a raw local transport.
pub fn local_openai_codex_provider(
    transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider(LocalProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds the local OpenAI Codex provider with a selectable WebSocket transport.
pub fn local_provider_with_websocket(
    inputs: LocalProviderInputs,
    websocket: Rc<dyn LocalOpenAiCodexWebSocketTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let api = agentprism_openai::local_openai_codex_responses_api_with_websocket(
        Rc::clone(&inputs.http),
        websocket,
    );
    build_local_provider(inputs.http, api)
}

/// Builds local OpenAI Codex directly with a selectable WebSocket transport.
pub fn local_openai_codex_provider_with_websocket(
    transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
    websocket: Rc<dyn LocalOpenAiCodexWebSocketTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider_with_websocket(
        LocalProviderInputs {
            http: transport,
            environment: Default::default(),
        },
        websocket,
    )
}

fn build_local_provider(
    transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
    api: Rc<dyn agentprism_ai::LocalChatApi>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let oauth: Rc<dyn LocalOAuthAuth> = Rc::new(LocalOpenAiCodexOAuth::new(transport));
    agentprism_ai::LocalProviderRegistration::builder("openai-codex")
        .display_name("OpenAI Codex")
        .base_url(
            Url::parse("https://chatgpt.com/backend-api")
                .map_err(ProviderBuildError::configuration)?,
        )
        .auth(Rc::new(LocalCodexAuthResolver::new(oauth)))
        .models(models()?)
        .api(agentprism_ai::OpenAiCodexResponses::API_ID, api)
        .retry_policy(openai_codex_retry_policy())
        .retry_classifier(Rc::new(LocalOpenAiCodexRetryClassifier::default()))
        .build()
        .map_err(ProviderBuildError::Registration)
}

struct CodexAuthResolver {
    access_token: OpenAiCodexAccessTokenAuth,
    inner: ProviderAuthResolver,
}

impl CodexAuthResolver {
    fn new(oauth: Arc<dyn OAuthAuth>) -> Self {
        Self {
            access_token: OpenAiCodexAccessTokenAuth,
            inner: ProviderAuthResolver::new(None, Some(oauth)),
        }
    }
}

impl AuthResolver for CodexAuthResolver {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        if let Some(key) = request.overrides.api_key.clone() {
            return agentprism_ai::ApiKeyAuth::resolve(
                &self.access_token,
                agentprism_ai::ApiKeyResolveRequest {
                    provider: request.provider,
                    credential: Some(agentprism_ai::ApiKeyCredential {
                        key: Some(key),
                        environment: request.overrides.environment.clone(),
                    }),
                    context: request.auth_context,
                    environment: request.overrides.environment,
                },
                cancellation,
            );
        }
        self.inner.resolve(request, cancellation)
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<agentprism_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }

    fn logout(&self, cancellation: CancellationToken) -> SendBoxFuture<'_, Result<(), AuthError>> {
        self.inner.logout(cancellation)
    }
}

struct LocalCodexAuthResolver {
    access_token: OpenAiCodexAccessTokenAuth,
    inner: LocalProviderAuthResolver,
}

impl LocalCodexAuthResolver {
    fn new(oauth: Rc<dyn LocalOAuthAuth>) -> Self {
        Self {
            access_token: OpenAiCodexAccessTokenAuth,
            inner: LocalProviderAuthResolver::new(None, Some(oauth)),
        }
    }
}

impl LocalAuthResolver for LocalCodexAuthResolver {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        if let Some(key) = request.overrides.api_key.clone() {
            return agentprism_ai::LocalApiKeyAuth::resolve(
                &self.access_token,
                agentprism_ai::LocalApiKeyResolveRequest {
                    provider: request.provider,
                    credential: Some(agentprism_ai::ApiKeyCredential {
                        key: Some(key),
                        environment: request.overrides.environment.clone(),
                    }),
                    context: request.auth_context,
                    environment: request.overrides.environment,
                },
                cancellation,
            );
        }
        self.inner.resolve(request, cancellation)
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<agentprism_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }

    fn logout(&self, cancellation: CancellationToken) -> LocalBoxFuture<'_, Result<(), AuthError>> {
        self.inner.logout(cancellation)
    }
}
