//! OpenAI Codex provider leaf backed by the shared Responses family.

#![deny(missing_docs)]

mod oauth;

use pi_ai::{
    ApiFamily, AuthError, AuthInteraction, AuthResolver, CancellationToken, LocalAuthInteraction,
    LocalAuthResolver, LocalBoxFuture, LocalOAuthAuth, LocalProviderAuthResolver,
    LocalResolveAuthRequest, OAuthAuth, ProviderAuthResolver, ResolveAuthRequest, ResolvedAuth,
    SendBoxFuture,
};
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

pub use oauth::{
    LocalOpenAiCodexOAuth, OpenAiCodexAccessTokenAuth, OpenAiCodexOAuth, account_id_from_jwt,
};
pub use pi_ai_openai::{
    LocalOpenAiCodexResponsesTransport, LocalOpenAiCodexRetryClassifier,
    LocalOpenAiCodexWebSocketResponse, LocalOpenAiCodexWebSocketTransport,
    OpenAiCodexResponsesTransport, OpenAiCodexRetryClassifier, OpenAiCodexWebSocketConnection,
    OpenAiCodexWebSocketRequest, OpenAiCodexWebSocketResponse, OpenAiCodexWebSocketTransport,
    local_openai_codex_responses_api, local_openai_codex_responses_api_with_websocket,
    openai_codex_responses_api, openai_codex_responses_api_with_websocket,
    openai_codex_retry_policy,
};
pub use pi_ai_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};

/// Returns the pinned OpenAI Codex catalog owned by this leaf.
pub fn models() -> Result<Vec<pi_ai::ModelDescriptor>, ProviderBuildError> {
    pi_ai_openai::parse_openai_published_catalog(
        include_str!("../data/models.json"),
        "openai-codex",
        "openai-codex-responses",
    )
    .map_err(ProviderBuildError::catalog)
}

/// Compatibility name for the leaf-owned OpenAI Codex catalog.
pub fn openai_codex_models() -> Result<Vec<pi_ai::ModelDescriptor>, ProviderBuildError> {
    models()
}

/// Builds the Send OpenAI Codex provider.
pub fn provider(inputs: ProviderInputs) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    let api = pi_ai_openai::openai_codex_responses_api(Arc::clone(&inputs.http));
    build_provider(inputs.http, api)
}

/// Builds OpenAI Codex directly from a raw Send transport.
pub fn openai_codex_provider(
    transport: Arc<dyn pi_ai::HttpTransport>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    provider(ProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds the Send OpenAI Codex provider with a selectable WebSocket transport.
pub fn provider_with_websocket(
    inputs: ProviderInputs,
    websocket: Arc<dyn OpenAiCodexWebSocketTransport>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    let api = pi_ai_openai::openai_codex_responses_api_with_websocket(
        Arc::clone(&inputs.http),
        websocket,
    );
    build_provider(inputs.http, api)
}

/// Builds OpenAI Codex directly with a selectable WebSocket transport.
pub fn openai_codex_provider_with_websocket(
    transport: Arc<dyn pi_ai::HttpTransport>,
    websocket: Arc<dyn OpenAiCodexWebSocketTransport>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    provider_with_websocket(
        ProviderInputs {
            http: transport,
            environment: Default::default(),
        },
        websocket,
    )
}

fn build_provider(
    transport: Arc<dyn pi_ai::HttpTransport>,
    api: Arc<dyn pi_ai::ChatApi>,
) -> Result<pi_ai::ProviderRegistration, ProviderBuildError> {
    let oauth: Arc<dyn OAuthAuth> = Arc::new(OpenAiCodexOAuth::new(transport));
    pi_ai::ProviderRegistration::builder("openai-codex")
        .display_name("OpenAI Codex")
        .base_url(
            Url::parse("https://chatgpt.com/backend-api")
                .map_err(ProviderBuildError::configuration)?,
        )
        .auth(Arc::new(CodexAuthResolver::new(oauth)))
        .models(models()?)
        .api(pi_ai::OpenAiCodexResponses::API_ID, api)
        .retry_policy(openai_codex_retry_policy())
        .retry_classifier(Arc::new(OpenAiCodexRetryClassifier::default()))
        .build()
        .map_err(ProviderBuildError::Registration)
}

/// Builds the local OpenAI Codex provider.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    let api = pi_ai_openai::local_openai_codex_responses_api(Rc::clone(&inputs.http));
    build_local_provider(inputs.http, api)
}

/// Builds OpenAI Codex directly from a raw local transport.
pub fn local_openai_codex_provider(
    transport: Rc<dyn pi_ai::LocalHttpTransport>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider(LocalProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds the local OpenAI Codex provider with a selectable WebSocket transport.
pub fn local_provider_with_websocket(
    inputs: LocalProviderInputs,
    websocket: Rc<dyn LocalOpenAiCodexWebSocketTransport>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    let api = pi_ai_openai::local_openai_codex_responses_api_with_websocket(
        Rc::clone(&inputs.http),
        websocket,
    );
    build_local_provider(inputs.http, api)
}

/// Builds local OpenAI Codex directly with a selectable WebSocket transport.
pub fn local_openai_codex_provider_with_websocket(
    transport: Rc<dyn pi_ai::LocalHttpTransport>,
    websocket: Rc<dyn LocalOpenAiCodexWebSocketTransport>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider_with_websocket(
        LocalProviderInputs {
            http: transport,
            environment: Default::default(),
        },
        websocket,
    )
}

fn build_local_provider(
    transport: Rc<dyn pi_ai::LocalHttpTransport>,
    api: Rc<dyn pi_ai::LocalChatApi>,
) -> Result<pi_ai::LocalProviderRegistration, ProviderBuildError> {
    let oauth: Rc<dyn LocalOAuthAuth> = Rc::new(LocalOpenAiCodexOAuth::new(transport));
    pi_ai::LocalProviderRegistration::builder("openai-codex")
        .display_name("OpenAI Codex")
        .base_url(
            Url::parse("https://chatgpt.com/backend-api")
                .map_err(ProviderBuildError::configuration)?,
        )
        .auth(Rc::new(LocalCodexAuthResolver::new(oauth)))
        .models(models()?)
        .api(pi_ai::OpenAiCodexResponses::API_ID, api)
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
            return pi_ai::ApiKeyAuth::resolve(
                &self.access_token,
                pi_ai::ApiKeyResolveRequest {
                    provider: request.provider,
                    credential: Some(pi_ai::ApiKeyCredential {
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
    ) -> SendBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
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
            return pi_ai::LocalApiKeyAuth::resolve(
                &self.access_token,
                pi_ai::LocalApiKeyResolveRequest {
                    provider: request.provider,
                    credential: Some(pi_ai::ApiKeyCredential {
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
    ) -> LocalBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }

    fn logout(&self, cancellation: CancellationToken) -> LocalBoxFuture<'_, Result<(), AuthError>> {
        self.inner.logout(cancellation)
    }
}
