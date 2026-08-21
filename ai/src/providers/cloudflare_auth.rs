use crate::auth::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyCredentialType, ApiKeyResolveInput, AuthError, AuthPrompt,
    AuthResult, ModelAuth, ProviderAuthInteraction,
};
use crate::types::ProviderEnv;
use crate::utils::abort::abort_reason;
use std::sync::Arc;

const CLOUDFLARE_API_KEY: &str = "CLOUDFLARE_API_KEY";
const CLOUDFLARE_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
const CLOUDFLARE_GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";

#[derive(Clone, Copy)]
enum CloudflareAuthKind {
    WorkersAi,
    AiGateway,
}

async fn resolve_value(
    name: &str,
    input: &ApiKeyResolveInput,
) -> Result<Option<String>, AuthError> {
    let from_credential = input.credential.as_ref().and_then(|credential| {
        if name == CLOUDFLARE_API_KEY {
            credential.key.clone()
        } else {
            credential
                .env
                .as_ref()
                .and_then(|env| env.get(name))
                .cloned()
        }
    });
    if from_credential.is_some() {
        return Ok(from_credential);
    }
    if input.signal.is_aborted() {
        return Err(AuthError::abort(abort_reason(input.signal.as_ref())));
    }
    let value = input.ctx.env(name.to_owned()).await?;
    if input.signal.is_aborted() {
        return Err(AuthError::abort(abort_reason(input.signal.as_ref())));
    }
    Ok(value)
}

async fn resolve_cloudflare_env(
    kind: CloudflareAuthKind,
    input: ApiKeyResolveInput,
) -> Result<Option<(String, ProviderEnv, String)>, AuthError> {
    let api_key = resolve_value(CLOUDFLARE_API_KEY, &input).await?;
    let account_id = resolve_value(CLOUDFLARE_ACCOUNT_ID, &input).await?;
    let gateway_id = match kind {
        CloudflareAuthKind::WorkersAi => None,
        CloudflareAuthKind::AiGateway => resolve_value(CLOUDFLARE_GATEWAY_ID, &input).await?,
    };
    let Some(api_key) = api_key.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let Some(account_id) = account_id.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if matches!(kind, CloudflareAuthKind::AiGateway)
        && gateway_id.as_ref().is_none_or(String::is_empty)
    {
        return Ok(None);
    }
    let mut env = ProviderEnv::from([(CLOUDFLARE_ACCOUNT_ID.to_owned(), account_id)]);
    if let Some(gateway_id) = gateway_id {
        env.insert(CLOUDFLARE_GATEWAY_ID.to_owned(), gateway_id);
    }
    let source = if input.credential.is_some() {
        "stored credential".to_owned()
    } else {
        CLOUDFLARE_API_KEY.to_owned()
    };
    Ok(Some((api_key, env, source)))
}

async fn prompt(
    interaction: &ProviderAuthInteraction,
    prompt: AuthPrompt,
) -> Result<String, AuthError> {
    if interaction.signal.is_aborted() {
        return Err(AuthError::abort(abort_reason(interaction.signal.as_ref())));
    }
    let value = interaction.interaction.prompt(prompt).await?;
    if interaction.signal.is_aborted() {
        return Err(AuthError::abort(abort_reason(interaction.signal.as_ref())));
    }
    Ok(value)
}

fn login(
    kind: CloudflareAuthKind,
    interaction: ProviderAuthInteraction,
) -> crate::auth::types::AuthFuture<ApiKeyCredential> {
    Box::pin(async move {
        let key = prompt(
            &interaction,
            AuthPrompt::Secret {
                message: "Enter Cloudflare API key".to_owned(),
                placeholder: None,
                signal: None,
            },
        )
        .await?;
        let account_id = prompt(
            &interaction,
            AuthPrompt::Text {
                message: "Enter Cloudflare account ID".to_owned(),
                placeholder: None,
                signal: None,
            },
        )
        .await?;
        let mut env = ProviderEnv::from([(CLOUDFLARE_ACCOUNT_ID.to_owned(), account_id)]);
        if matches!(kind, CloudflareAuthKind::AiGateway) {
            let gateway_id = prompt(
                &interaction,
                AuthPrompt::Text {
                    message: "Enter Cloudflare AI Gateway ID".to_owned(),
                    placeholder: None,
                    signal: None,
                },
            )
            .await?;
            env.insert(CLOUDFLARE_GATEWAY_ID.to_owned(), gateway_id);
        }
        Ok(ApiKeyCredential {
            kind: ApiKeyCredentialType::ApiKey,
            key: Some(key),
            env: Some(env),
        })
    })
}

pub fn cloudflare_workers_ai_auth() -> ApiKeyAuth {
    ApiKeyAuth {
        name: "Cloudflare API key".to_owned(),
        login: Some(Arc::new(|interaction| {
            login(CloudflareAuthKind::WorkersAi, interaction)
        })),
        check: None,
        resolve: Arc::new(|input| {
            Box::pin(async move {
                let Some((api_key, env, source)) =
                    resolve_cloudflare_env(CloudflareAuthKind::WorkersAi, input).await?
                else {
                    return Ok(None);
                };
                Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(api_key),
                        ..Default::default()
                    },
                    env: Some(env),
                    source: Some(source),
                }))
            })
        }),
    }
}

pub fn cloudflare_ai_gateway_auth() -> ApiKeyAuth {
    ApiKeyAuth {
        name: "Cloudflare API key".to_owned(),
        login: Some(Arc::new(|interaction| {
            login(CloudflareAuthKind::AiGateway, interaction)
        })),
        check: None,
        resolve: Arc::new(|input| {
            Box::pin(async move {
                let Some((api_key, env, source)) =
                    resolve_cloudflare_env(CloudflareAuthKind::AiGateway, input).await?
                else {
                    return Ok(None);
                };
                Ok(Some(AuthResult {
                    auth: ModelAuth {
                        headers: Some(crate::types::ProviderHeaders::from([
                            (
                                "cf-aig-authorization".to_owned(),
                                Some(format!("Bearer {api_key}")),
                            ),
                            ("Authorization".to_owned(), None),
                            ("x-api-key".to_owned(), None),
                        ])),
                        ..Default::default()
                    },
                    env: Some(env),
                    source: Some(source),
                }))
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::types::{AuthContext, AuthFuture};
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
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect(),
            )),
            credential,
            signal: AbortController::new().signal(),
        }
    }

    /// Ports pi `test/providers.test.ts:169-211` and
    /// `src/providers/cloudflare-auth.ts:11-55`'s per-field merge.
    #[tokio::test]
    async fn cloudflare_resolution_requires_scoped_ids_and_merges_stored_fields() {
        let workers = cloudflare_workers_ai_auth();
        assert!(
            (workers.resolve)(input([(CLOUDFLARE_API_KEY, "cf-key")], None))
                .await
                .expect("resolve")
                .is_none()
        );
        let stored_key = ApiKeyCredential {
            kind: ApiKeyCredentialType::ApiKey,
            key: Some("stored-key".to_owned()),
            env: None,
        };
        let resolved = (workers.resolve)(input(
            [
                (CLOUDFLARE_API_KEY, "ambient-key"),
                (CLOUDFLARE_ACCOUNT_ID, "account-id"),
            ],
            Some(stored_key),
        ))
        .await
        .expect("resolve")
        .expect("configured");
        assert_eq!(resolved.auth.api_key.as_deref(), Some("stored-key"));
        assert_eq!(
            resolved
                .env
                .as_ref()
                .and_then(|env| env.get(CLOUDFLARE_ACCOUNT_ID)),
            Some(&"account-id".to_owned())
        );
        assert_eq!(resolved.source.as_deref(), Some("stored credential"));

        let gateway = cloudflare_ai_gateway_auth();
        assert!(
            (gateway.resolve)(input(
                [
                    (CLOUDFLARE_API_KEY, "cf-key"),
                    (CLOUDFLARE_ACCOUNT_ID, "account-id"),
                ],
                None,
            ))
            .await
            .expect("resolve")
            .is_none()
        );
        let resolved = (gateway.resolve)(input(
            [
                (CLOUDFLARE_API_KEY, "cf-key"),
                (CLOUDFLARE_ACCOUNT_ID, "account-id"),
                (CLOUDFLARE_GATEWAY_ID, "gateway-id"),
            ],
            None,
        ))
        .await
        .expect("resolve")
        .expect("configured");
        assert_eq!(
            resolved.auth.headers,
            Some(crate::types::ProviderHeaders::from([
                (
                    "cf-aig-authorization".to_owned(),
                    Some("Bearer cf-key".to_owned()),
                ),
                ("Authorization".to_owned(), None),
                ("x-api-key".to_owned(), None),
            ]))
        );
        assert_eq!(
            resolved.env,
            Some(ProviderEnv::from([
                (CLOUDFLARE_ACCOUNT_ID.to_owned(), "account-id".to_owned()),
                (CLOUDFLARE_GATEWAY_ID.to_owned(), "gateway-id".to_owned()),
            ]))
        );
    }
}
