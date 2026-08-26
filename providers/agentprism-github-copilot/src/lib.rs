//! GitHub Copilot provider leaf with concrete OAuth and entitlement filtering.

#![deny(missing_docs)]

mod oauth;

use agentprism_ai::{
    AiError, ApiId, ApiRequestOptions, AssistantStream, AuthResolutionOverrides, CancellationToken,
    ChatApi, Context, Credential, ErasedApiFullOptions, LocalAssistantStream, LocalBoxFuture,
    LocalChatApi, LocalResolvedApiRequest, Message, ModelDescriptor, OAuthCredential,
    ProviderOAuthExtra, ResolvedApiRequest, SendBoxFuture, ToolResultContent,
};
use http::{HeaderMap, HeaderValue};
use std::rc::Rc;
use std::sync::Arc;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pub use oauth::{GitHubCopilotOAuth, LocalGitHubCopilotOAuth};

fn load_catalog() -> Result<Vec<ModelDescriptor>, String> {
    let source = include_str!("../data/models.json");
    let mut models = agentprism_anthropic::parse_anthropic_published_catalog(source)
        .map_err(|error| error.to_string())?;
    for api in ["openai-completions", "openai-responses"] {
        models.extend(
            agentprism_openai::parse_openai_published_catalog(source, "github-copilot", api)
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(models)
}

/// Returns the complete pinned GitHub Copilot catalog owned by this leaf.
pub fn models() -> Result<Vec<ModelDescriptor>, ProviderBuildError> {
    load_catalog().map_err(ProviderBuildError::catalog)
}

/// Applies pinned Copilot OAuth entitlement narrowing.
pub fn filter_entitled_models(
    models: &[ModelDescriptor],
    credential: Option<&Credential>,
) -> Vec<ModelDescriptor> {
    let Some(Credential::OAuth(OAuthCredential {
        extra:
            ProviderOAuthExtra::GitHubCopilot {
                available_model_ids: Some(available),
                ..
            },
        ..
    })) = credential
    else {
        return models.to_vec();
    };
    models
        .iter()
        .filter(|model| available.contains(&model.common.model_ref.model))
        .cloned()
        .collect()
}

fn copilot_api(inner: Arc<dyn ChatApi>) -> Arc<dyn ChatApi> {
    Arc::new(CopilotApi { inner })
}

fn local_copilot_api(inner: Rc<dyn LocalChatApi>) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalCopilotApi { inner })
}

struct CopilotApi {
    inner: Arc<dyn ChatApi>,
}

impl ChatApi for CopilotApi {
    fn apply_full_options_auth_overrides(
        &self,
        model: &ModelDescriptor,
        options: &ErasedApiFullOptions,
        overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        self.inner
            .apply_full_options_auth_overrides(model, options, overrides)
    }

    fn apply_full_options_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        effective_base_url: &url::Url,
        request_options: &ApiRequestOptions,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        self.inner.apply_full_options_headers(
            model,
            context,
            options,
            effective_base_url,
            request_options,
            headers,
        )
    }

    fn apply_contextual_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        effective_base_url: &url::Url,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        self.inner
            .apply_contextual_headers(model, context, effective_base_url, headers)?;
        apply_copilot_request_headers(context, headers);
        Ok(())
    }

    fn stream(
        &self,
        request: ResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, AiError>> {
        self.inner.stream(request, cancellation)
    }
}

struct LocalCopilotApi {
    inner: Rc<dyn LocalChatApi>,
}

impl LocalChatApi for LocalCopilotApi {
    fn apply_full_options_auth_overrides(
        &self,
        model: &ModelDescriptor,
        options: &ErasedApiFullOptions,
        overrides: &mut AuthResolutionOverrides,
    ) -> Result<(), AiError> {
        self.inner
            .apply_full_options_auth_overrides(model, options, overrides)
    }

    fn apply_full_options_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        effective_base_url: &url::Url,
        request_options: &ApiRequestOptions,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        self.inner.apply_full_options_headers(
            model,
            context,
            options,
            effective_base_url,
            request_options,
            headers,
        )
    }

    fn apply_contextual_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        effective_base_url: &url::Url,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        self.inner
            .apply_contextual_headers(model, context, effective_base_url, headers)?;
        apply_copilot_request_headers(context, headers);
        Ok(())
    }

    fn stream(
        &self,
        request: LocalResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, AiError>> {
        self.inner.stream(request, cancellation)
    }
}

fn apply_copilot_request_headers(context: &Context, headers: &mut HeaderMap) {
    let initiator = match context.messages.last() {
        Some(Message::User(_)) | None => "user",
        Some(Message::Assistant(_) | Message::ToolResult(_)) => "agent",
    };
    headers.insert("x-initiator", HeaderValue::from_static(initiator));
    headers.insert(
        "openai-intent",
        HeaderValue::from_static("conversation-edits"),
    );
    if has_copilot_vision_input(context) {
        headers.insert("copilot-vision-request", HeaderValue::from_static("true"));
    }
}

fn has_copilot_vision_input(context: &Context) -> bool {
    context.messages.iter().any(|message| match message {
        Message::User(message) => message
            .content
            .iter()
            .any(|block| matches!(block, agentprism_ai::ContentBlock::Image { .. })),
        Message::ToolResult(message) => message
            .content
            .iter()
            .any(|block| matches!(block, ToolResultContent::Image { .. })),
        Message::Assistant(_) => false,
    })
}

/// Builds the Send GitHub Copilot provider registration.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let oauth = Arc::new(GitHubCopilotOAuth::new(Arc::clone(&inputs.http)));
    agentprism_ai::ProviderRegistration::builder("github-copilot")
        .display_name("GitHub Copilot")
        .auth(agentprism_provider_common::bearer_auth(
            "GitHub Copilot token",
            "COPILOT_GITHUB_TOKEN",
            Some(oauth),
        ))
        .models(models()?)
        .filter_models(Arc::new(filter_entitled_models))
        .api(
            ApiId::new("anthropic-messages"),
            copilot_api(agentprism_anthropic::anthropic_messages_api(Arc::clone(
                &inputs.http,
            ))),
        )
        .api(
            ApiId::new("openai-completions"),
            copilot_api(agentprism_openai::openai_completions_api(Arc::clone(
                &inputs.http,
            ))),
        )
        .api(
            ApiId::new("openai-responses"),
            copilot_api(agentprism_openai::openai_responses_api(inputs.http)),
        )
        .build()
        .map_err(ProviderBuildError::Registration)
}

/// Builds the local-executor GitHub Copilot provider registration.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let oauth = Rc::new(LocalGitHubCopilotOAuth::new(Rc::clone(&inputs.http)));
    agentprism_ai::LocalProviderRegistration::builder("github-copilot")
        .display_name("GitHub Copilot")
        .auth(agentprism_provider_common::local_bearer_auth(
            "GitHub Copilot token",
            "COPILOT_GITHUB_TOKEN",
            Some(oauth),
        ))
        .models(models()?)
        .filter_models(Rc::new(filter_entitled_models))
        .api(
            ApiId::new("anthropic-messages"),
            local_copilot_api(agentprism_anthropic::local_anthropic_messages_api(
                Rc::clone(&inputs.http),
            )),
        )
        .api(
            ApiId::new("openai-completions"),
            local_copilot_api(agentprism_openai::local_openai_completions_api(Rc::clone(
                &inputs.http,
            ))),
        )
        .api(
            ApiId::new("openai-responses"),
            local_copilot_api(agentprism_openai::local_openai_responses_api(inputs.http)),
        )
        .build()
        .map_err(ProviderBuildError::Registration)
}
