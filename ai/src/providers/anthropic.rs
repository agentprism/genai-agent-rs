use super::{anthropic_models::ANTHROPIC_MODELS, static_provider};
use crate::api::anthropic_messages::anthropic_messages_api;
use crate::auth::oauth::load::load_anthropic_oauth;
use crate::auth::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyCredentialType, ApiKeyResolveInput, AuthError, AuthPrompt,
    AuthResult, ModelAuth, ProviderAuth, ProviderAuthInteraction,
};
use crate::auth::{AuthFuture, lazy_oauth};
use crate::models::ProviderRef;
use crate::utils::abort::abort_reason;
use std::sync::Arc;

async fn read_env(input: &ApiKeyResolveInput, name: &str) -> Result<Option<String>, AuthError> {
    let value = input.ctx.env(name.to_owned()).await?;
    if input.signal.is_aborted() {
        return Err(AuthError::abort(abort_reason(input.signal.as_ref())));
    }
    Ok(value.filter(|value| !value.is_empty()))
}

fn anthropic_api_key_auth() -> ApiKeyAuth {
    ApiKeyAuth {
        name: "Anthropic API key".to_owned(),
        login: Some(Arc::new(|interaction: ProviderAuthInteraction| {
            Box::pin(async move {
                if interaction.signal.is_aborted() {
                    return Err(AuthError::abort(abort_reason(interaction.signal.as_ref())));
                }
                let key = interaction
                    .interaction
                    .prompt(AuthPrompt::Secret {
                        message: "Enter Anthropic API key".to_owned(),
                        placeholder: None,
                        signal: None,
                    })
                    .await?;
                if interaction.signal.is_aborted() {
                    return Err(AuthError::abort(abort_reason(interaction.signal.as_ref())));
                }
                Ok(ApiKeyCredential {
                    kind: ApiKeyCredentialType::ApiKey,
                    key: Some(key),
                    env: None,
                })
            }) as AuthFuture<ApiKeyCredential>
        })),
        check: None,
        resolve: Arc::new(|input| {
            Box::pin(async move {
                if input.signal.is_aborted() {
                    return Err(AuthError::abort(abort_reason(input.signal.as_ref())));
                }
                if let Some(credential) = &input.credential
                    && let Some(key) = credential.key.as_ref().filter(|key| !key.is_empty())
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth {
                            api_key: Some(key.clone()),
                            ..Default::default()
                        },
                        env: credential.env.clone(),
                        source: Some("stored credential".to_owned()),
                    }));
                }
                if let Some(token) = read_env(&input, "ANTHROPIC_AUTH_TOKEN").await? {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth {
                            headers: Some(crate::types::ProviderHeaders::from([(
                                "Authorization".to_owned(),
                                Some(format!("Bearer {token}")),
                            )])),
                            ..Default::default()
                        },
                        env: None,
                        source: Some("ANTHROPIC_AUTH_TOKEN".to_owned()),
                    }));
                }
                for name in ["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
                    if let Some(key) = read_env(&input, name).await? {
                        return Ok(Some(AuthResult {
                            auth: ModelAuth {
                                api_key: Some(key),
                                ..Default::default()
                            },
                            env: None,
                            source: Some(name.to_owned()),
                        }));
                    }
                }
                Ok(None)
            })
        }),
    }
}

pub fn anthropic_provider() -> ProviderRef {
    static_provider(
        "anthropic",
        "Anthropic",
        "https://api.anthropic.com",
        ProviderAuth {
            api_key: Some(anthropic_api_key_auth()),
            oauth: Some(lazy_oauth(
                "Anthropic (Claude Pro/Max)".to_owned(),
                Some(true),
                None,
                Arc::new(load_anthropic_oauth),
            )),
        },
        &ANTHROPIC_MODELS,
        Arc::new(anthropic_messages_api()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthContext, AuthFuture};
    use crate::types::ProviderEnv;
    use crate::utils::abort::AbortController;
    use indexmap::IndexMap;

    #[derive(Clone, Default)]
    struct StaticContext(IndexMap<String, String>);

    impl AuthContext for StaticContext {
        fn env(&self, name: String) -> AuthFuture<Option<String>> {
            let value = self.0.get(&name).cloned();
            Box::pin(async move { Ok(value) })
        }

        fn file_exists(&self, _path: String) -> AuthFuture<bool> {
            Box::pin(async { Ok(false) })
        }
    }

    fn input(
        values: impl IntoIterator<Item = (&'static str, &'static str)>,
        credential: Option<ApiKeyCredential>,
    ) -> ApiKeyResolveInput {
        ApiKeyResolveInput {
            ctx: Arc::new(StaticContext(
                values
                    .into_iter()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect(),
            )),
            credential,
            signal: AbortController::new().signal(),
        }
    }

    /// Ports pi `test/anthropic-auth-token.test.ts:8-215` and
    /// `src/providers/anthropic.ts:25-48`.
    #[tokio::test]
    async fn stored_key_and_auth_token_precedence_match_pi() {
        let auth = anthropic_api_key_auth();
        let stored_env = ProviderEnv::from([("REGION".to_owned(), "west".to_owned())]);
        let resolved = (auth.resolve)(input(
            [
                ("ANTHROPIC_AUTH_TOKEN", "auth-token"),
                ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
                ("ANTHROPIC_API_KEY", "api-key"),
            ],
            Some(ApiKeyCredential {
                kind: ApiKeyCredentialType::ApiKey,
                key: Some("stored".to_owned()),
                env: Some(stored_env.clone()),
            }),
        ))
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(resolved.auth.api_key.as_deref(), Some("stored"));
        assert_eq!(resolved.env, Some(stored_env));
        assert_eq!(resolved.source.as_deref(), Some("stored credential"));

        let resolved = (auth.resolve)(input(
            [
                ("ANTHROPIC_AUTH_TOKEN", "auth-token"),
                ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
                ("ANTHROPIC_API_KEY", "api-key"),
            ],
            None,
        ))
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(resolved.auth.api_key, None);
        assert_eq!(
            resolved.auth.headers,
            Some(crate::types::ProviderHeaders::from([(
                "Authorization".to_owned(),
                Some("Bearer auth-token".to_owned()),
            )]))
        );
        assert_eq!(resolved.source.as_deref(), Some("ANTHROPIC_AUTH_TOKEN"));
    }

    /// Pins pi `src/providers/anthropic.ts:39-43` fallback order.
    #[tokio::test]
    async fn oauth_token_precedes_api_key_when_auth_token_is_absent() {
        let resolved = (anthropic_api_key_auth().resolve)(input(
            [
                ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
                ("ANTHROPIC_API_KEY", "api-key"),
            ],
            None,
        ))
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(resolved.auth.api_key.as_deref(), Some("oauth-token"));
        assert_eq!(resolved.source.as_deref(), Some("ANTHROPIC_OAUTH_TOKEN"));
    }
}
