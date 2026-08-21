use super::amazon_bedrock_models::AMAZON_BEDROCK_MODELS;
use crate::api::bedrock_converse_stream::bedrock_converse_stream_api;
use crate::auth::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyCredentialType, ApiKeyResolveInput, AuthError, AuthEvent,
    AuthFuture, AuthInfoLink, AuthPrompt, AuthResult, AuthSelectOption, ModelAuth, ProviderAuth,
    ProviderAuthInteraction,
};
use crate::models::{CreateProviderOptions, ProviderApi, ProviderRef, create_provider};
use crate::types::ProviderEnv;
use crate::utils::abort::abort_reason;
use std::sync::Arc;

fn ensure_not_aborted(signal: &Arc<dyn crate::types::AbortSignal>) -> Result<(), AuthError> {
    if signal.is_aborted() {
        Err(AuthError::abort(abort_reason(signal.as_ref())))
    } else {
        Ok(())
    }
}

fn bedrock_login(interaction: ProviderAuthInteraction) -> AuthFuture<ApiKeyCredential> {
    Box::pin(async move {
        ensure_not_aborted(&interaction.signal)?;
        let method = interaction
            .interaction
            .prompt(AuthPrompt::Select {
                message: "Select Amazon Bedrock authentication method:".to_owned(),
                options: vec![
                    AuthSelectOption {
                        id: "bearer-token".to_owned(),
                        label: "Bearer token".to_owned(),
                        description: None,
                    },
                    AuthSelectOption {
                        id: "aws-profile".to_owned(),
                        label: "AWS profile".to_owned(),
                        description: None,
                    },
                    AuthSelectOption {
                        id: "credential-chain".to_owned(),
                        label: "Existing AWS credential chain".to_owned(),
                        description: None,
                    },
                ],
                signal: None,
            })
            .await?;
        ensure_not_aborted(&interaction.signal)?;
        if method == "bearer-token" {
            let key = interaction
                .interaction
                .prompt(AuthPrompt::Secret {
                    message: "Enter Amazon Bedrock bearer token".to_owned(),
                    placeholder: None,
                    signal: None,
                })
                .await?;
            return Ok(ApiKeyCredential {
                kind: ApiKeyCredentialType::ApiKey,
                key: Some(key),
                env: None,
            });
        }
        interaction.interaction.notify(AuthEvent::Info {
            message:
                "Amazon Bedrock supports AWS profiles, IAM credentials, and role-based credentials."
                    .to_owned(),
            links: Some(vec![AuthInfoLink {
                label: Some("AWS credential provider chain".to_owned()),
                url:
                    "https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html"
                        .to_owned(),
            }]),
        });
        if method == "aws-profile" {
            let profile = interaction
                .interaction
                .prompt(AuthPrompt::Text {
                    message: "Enter AWS profile name".to_owned(),
                    placeholder: None,
                    signal: None,
                })
                .await?;
            return Ok(ApiKeyCredential {
                kind: ApiKeyCredentialType::ApiKey,
                key: None,
                env: Some(ProviderEnv::from([("AWS_PROFILE".to_owned(), profile)])),
            });
        }
        if method != "credential-chain" {
            return Err(AuthError::new(format!(
                "Unknown Amazon Bedrock auth method: {method}"
            )));
        }
        interaction
            .interaction
            .prompt(AuthPrompt::Text {
                message: "Configure AWS credentials, then press Enter to continue".to_owned(),
                placeholder: None,
                signal: None,
            })
            .await?;
        Ok(ApiKeyCredential::default())
    })
}

async fn read_env(input: &ApiKeyResolveInput, name: &str) -> Result<Option<String>, AuthError> {
    ensure_not_aborted(&input.signal)?;
    let value = input.ctx.env(name.to_owned()).await?;
    ensure_not_aborted(&input.signal)?;
    Ok(value)
}

pub fn bedrock_auth() -> ApiKeyAuth {
    ApiKeyAuth {
        name: "AWS credentials or bearer token".to_owned(),
        login: Some(Arc::new(bedrock_login)),
        check: None,
        resolve: Arc::new(|input| {
            Box::pin(async move {
                if let Some(key) = input
                    .credential
                    .as_ref()
                    .and_then(|credential| credential.key.as_ref())
                    .filter(|key| !key.is_empty())
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth {
                            api_key: Some(key.clone()),
                            ..ModelAuth::default()
                        },
                        env: input
                            .credential
                            .as_ref()
                            .and_then(|credential| credential.env.clone()),
                        source: Some("stored credential".to_owned()),
                    }));
                }
                if read_env(&input, "AWS_BEARER_TOKEN_BEDROCK")
                    .await?
                    .is_some_and(|value| !value.is_empty())
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: None,
                        source: Some("AWS_BEARER_TOKEN_BEDROCK".to_owned()),
                    }));
                }
                let stored_profile = input
                    .credential
                    .as_ref()
                    .and_then(|credential| credential.env.as_ref())
                    .and_then(|env| env.get("AWS_PROFILE"))
                    .cloned();
                let profile = if let Some(profile) = &stored_profile {
                    Some(profile.clone())
                } else {
                    read_env(&input, "AWS_PROFILE").await?
                };
                if profile.is_some_and(|value| !value.is_empty()) {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: input
                            .credential
                            .as_ref()
                            .and_then(|credential| credential.env.clone()),
                        source: Some(if stored_profile.is_some() {
                            "stored credential".to_owned()
                        } else {
                            "AWS_PROFILE".to_owned()
                        }),
                    }));
                }
                let access_key = read_env(&input, "AWS_ACCESS_KEY_ID").await?;
                if access_key.is_some_and(|value| !value.is_empty())
                    && read_env(&input, "AWS_SECRET_ACCESS_KEY")
                        .await?
                        .is_some_and(|value| !value.is_empty())
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: None,
                        source: Some("AWS access keys".to_owned()),
                    }));
                }
                for name in [
                    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                ] {
                    if read_env(&input, name)
                        .await?
                        .is_some_and(|value| !value.is_empty())
                    {
                        return Ok(Some(AuthResult {
                            auth: ModelAuth::default(),
                            env: None,
                            source: Some("ECS task role".to_owned()),
                        }));
                    }
                }
                if read_env(&input, "AWS_WEB_IDENTITY_TOKEN_FILE")
                    .await?
                    .is_some_and(|value| !value.is_empty())
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: None,
                        source: Some("web identity token".to_owned()),
                    }));
                }
                Ok(None)
            })
        }),
    }
}

pub fn amazon_bedrock_provider() -> ProviderRef {
    create_provider(CreateProviderOptions {
        id: "amazon-bedrock".to_owned(),
        name: Some("Amazon Bedrock".to_owned()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(bedrock_auth()),
            oauth: None,
        },
        models: AMAZON_BEDROCK_MODELS.values().cloned().collect(),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(Arc::new(bedrock_converse_stream_api())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::types::{AuthInteraction, AuthPrompt};
    use crate::auth::{AuthContext, AuthFuture};
    use crate::utils::abort::AbortController;
    use indexmap::IndexMap;
    use std::collections::VecDeque;
    use std::sync::Mutex;

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

    struct QueueInteraction {
        answers: Mutex<VecDeque<String>>,
        events: Mutex<Vec<AuthEvent>>,
    }

    impl QueueInteraction {
        fn new(answers: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().map(str::to_owned).collect()),
                events: Mutex::new(Vec::new()),
            }
        }
    }

    impl AuthInteraction for QueueInteraction {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            None
        }

        fn prompt(&self, _prompt: AuthPrompt) -> AuthFuture<String> {
            let answer = self
                .answers
                .lock()
                .expect("answers lock")
                .pop_front()
                .expect("queued answer");
            Box::pin(async move { Ok(answer) })
        }

        fn notify(&self, event: AuthEvent) {
            self.events.lock().expect("events lock").push(event);
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

    /// Ports pi `test/providers.test.ts:121-155` and pins the credential-chain branch from
    /// `src/providers/amazon-bedrock.ts:29-51`.
    #[tokio::test]
    async fn provider_owned_login_flows_return_bearer_profile_and_chain_credentials() {
        let login = bedrock_auth().login.expect("Bedrock login");
        let bearer = login(ProviderAuthInteraction {
            interaction: Arc::new(QueueInteraction::new(["bearer-token", "bedrock-token"])),
            signal: AbortController::new().signal(),
        })
        .await
        .expect("bearer login");
        assert_eq!(bearer.key.as_deref(), Some("bedrock-token"));
        assert_eq!(bearer.env, None);

        let profile_interaction = Arc::new(QueueInteraction::new(["aws-profile", "work"]));
        let profile = login(ProviderAuthInteraction {
            interaction: profile_interaction.clone(),
            signal: AbortController::new().signal(),
        })
        .await
        .expect("profile login");
        assert_eq!(
            profile.env,
            Some(ProviderEnv::from([(
                "AWS_PROFILE".to_owned(),
                "work".to_owned()
            )]))
        );
        assert!(matches!(
            profile_interaction
                .events
                .lock()
                .expect("events lock")
                .as_slice(),
            [AuthEvent::Info { links: Some(links), .. }]
                if links[0].label.as_deref() == Some("AWS credential provider chain")
        ));

        let chain = login(ProviderAuthInteraction {
            interaction: Arc::new(QueueInteraction::new(["credential-chain", "ready"])),
            signal: AbortController::new().signal(),
        })
        .await
        .expect("credential-chain login");
        assert_eq!(chain, ApiKeyCredential::default());
    }

    /// Pins pi `src/providers/amazon-bedrock.ts:54-78`.
    #[tokio::test]
    async fn auth_resolution_preserves_credential_chain_precedence() {
        let auth = bedrock_auth();
        let stored_env = ProviderEnv::from([("AWS_PROFILE".to_owned(), "stored".to_owned())]);
        let resolved = (auth.resolve)(input(
            [
                ("AWS_BEARER_TOKEN_BEDROCK", "ambient-token"),
                ("AWS_PROFILE", "ambient-profile"),
            ],
            Some(ApiKeyCredential {
                kind: ApiKeyCredentialType::ApiKey,
                key: Some("stored-token".to_owned()),
                env: Some(stored_env.clone()),
            }),
        ))
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(resolved.auth.api_key.as_deref(), Some("stored-token"));
        assert_eq!(resolved.env, Some(stored_env));
        assert_eq!(resolved.source.as_deref(), Some("stored credential"));

        let resolved = (auth.resolve)(input(
            [
                ("AWS_BEARER_TOKEN_BEDROCK", "ambient-token"),
                ("AWS_PROFILE", "ambient-profile"),
            ],
            None,
        ))
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(resolved.auth, ModelAuth::default());
        assert_eq!(resolved.source.as_deref(), Some("AWS_BEARER_TOKEN_BEDROCK"));

        let resolved = (auth.resolve)(input(
            [
                ("AWS_ACCESS_KEY_ID", "access"),
                ("AWS_SECRET_ACCESS_KEY", "secret"),
                ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/ecs"),
            ],
            None,
        ))
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(resolved.source.as_deref(), Some("AWS access keys"));

        let resolved = (auth.resolve)(input(
            [("AWS_CONTAINER_CREDENTIALS_FULL_URI", "https://ecs.test")],
            None,
        ))
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(resolved.source.as_deref(), Some("ECS task role"));

        let resolved = (auth.resolve)(input([("AWS_WEB_IDENTITY_TOKEN_FILE", "/token")], None))
            .await
            .expect("resolve")
            .expect("auth");
        assert_eq!(resolved.source.as_deref(), Some("web identity token"));

        assert_eq!(
            (auth.resolve)(input([], None)).await.expect("resolve"),
            None
        );
    }

    /// Pins pi `src/providers/amazon-bedrock.ts:65-70` nullish stored-profile behavior.
    #[tokio::test]
    async fn empty_stored_profile_does_not_fall_back_to_ambient_profile() {
        let unresolved = (bedrock_auth().resolve)(input(
            [("AWS_PROFILE", "ambient-profile")],
            Some(ApiKeyCredential {
                kind: ApiKeyCredentialType::ApiKey,
                key: None,
                env: Some(ProviderEnv::from([(
                    "AWS_PROFILE".to_owned(),
                    String::new(),
                )])),
            }),
        ))
        .await
        .expect("resolve");
        assert_eq!(unresolved, None);
    }

    /// Ports the non-network assertions from pi `test/bedrock-models.test.ts:29-38`.
    #[test]
    fn catalog_is_nonempty_and_opus_five_is_profile_only() {
        assert!(!AMAZON_BEDROCK_MODELS.is_empty());
        assert!(
            AMAZON_BEDROCK_MODELS
                .values()
                .any(|model| model.id == "global.anthropic.claude-opus-5")
        );
        assert!(
            !AMAZON_BEDROCK_MODELS
                .values()
                .any(|model| model.id == "anthropic.claude-opus-5")
        );
    }
}
