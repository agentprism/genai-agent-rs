//! Standard auth helpers ⇐ pi `src/auth/helpers.ts`.

use super::types::*;
use crate::utils::abort::{abort_reason, operation_signal};
use std::sync::Arc;
use tokio::sync::OnceCell;

pub fn env_api_key_auth(name: impl Into<String>, env_vars: Vec<String>) -> ApiKeyAuth {
    let name = name.into();
    let login_name = name.clone();
    let login = Arc::new(move |interaction: ProviderAuthInteraction| {
        let name = login_name.clone();
        Box::pin(async move {
            if interaction.signal.is_aborted() {
                return Err(AuthError::abort(abort_reason(interaction.signal.as_ref())));
            }
            let key = interaction
                .interaction
                .prompt(AuthPrompt::Secret {
                    message: format!("Enter {name}"),
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
    });
    let resolve = Arc::new(move |input: ApiKeyResolveInput| {
        let env_vars = env_vars.clone();
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
                        ..ModelAuth::default()
                    },
                    env: credential.env.clone(),
                    source: Some("stored credential".to_owned()),
                }));
            }
            for env_var in env_vars {
                let value = input.ctx.env(env_var.clone()).await?;
                if input.signal.is_aborted() {
                    return Err(AuthError::abort(abort_reason(input.signal.as_ref())));
                }
                if let Some(value) = value.filter(|value| !value.is_empty()) {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth {
                            api_key: Some(value),
                            ..ModelAuth::default()
                        },
                        env: None,
                        source: Some(env_var),
                    }));
                }
            }
            Ok(None)
        }) as AuthFuture<Option<AuthResult>>
    });
    ApiKeyAuth {
        name,
        login: Some(login),
        check: None,
        resolve,
    }
}

pub type OAuthLoader = Arc<dyn Fn() -> AuthFuture<OAuthAuth> + Send + Sync>;

pub fn lazy_oauth(
    name: String,
    is_subscription: Option<bool>,
    login_label: Option<String>,
    load: OAuthLoader,
) -> OAuthAuth {
    let loaded = Arc::new(OnceCell::<Result<OAuthAuth, AuthError>>::new());
    let get = move || {
        let loaded = loaded.clone();
        let load = load.clone();
        async move {
            loaded
                .get_or_init(|| async move { load().await })
                .await
                .clone()
        }
    };
    let get = Arc::new(get);
    let login_get = get.clone();
    let refresh_get = get.clone();
    let to_auth_get = get;
    OAuthAuth {
        name,
        is_subscription,
        login_label,
        login: Arc::new(move |interaction| {
            let get = login_get.clone();
            Box::pin(async move { ((get().await?).login)(interaction).await })
        }),
        refresh: Arc::new(move |credential, signal| {
            let get = refresh_get.clone();
            Box::pin(async move { ((get().await?).refresh)(credential, signal).await })
        }),
        to_auth: Arc::new(move |credential| {
            let get = to_auth_get.clone();
            Box::pin(async move { ((get().await?).to_auth)(credential).await })
        }),
    }
}

pub fn normalize_interaction(interaction: Arc<dyn AuthInteraction>) -> ProviderAuthInteraction {
    ProviderAuthInteraction {
        signal: operation_signal(interaction.signal()),
        interaction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::context::default_provider_auth_context;
    use crate::utils::abort::AbortController;
    use serde_json::Map;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn oauth_credential() -> OAuthCredential {
        OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "refresh".to_owned(),
            access: "access".to_owned(),
            expires: f64::MAX,
            extra: Map::new(),
        }
    }

    /// Pins pi `src/auth/helpers.ts:10-35`.
    #[tokio::test]
    async fn stored_api_key_wins_and_carries_credential_env() {
        let auth = env_api_key_auth("API key", vec!["__PI_AUTH_MISSING__".to_owned()]);
        let credential_env =
            crate::types::ProviderEnv::from([("REGION".to_owned(), "west".to_owned())]);
        let result = (auth.resolve)(ApiKeyResolveInput {
            ctx: Arc::new(default_provider_auth_context()),
            credential: Some(ApiKeyCredential {
                kind: ApiKeyCredentialType::ApiKey,
                key: Some("stored".to_owned()),
                env: Some(credential_env.clone()),
            }),
            signal: AbortController::new().signal(),
        })
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(result.auth.api_key.as_deref(), Some("stored"));
        assert_eq!(result.env, Some(credential_env));
        assert_eq!(result.source.as_deref(), Some("stored credential"));
    }

    /// Pins pi `src/auth/helpers.ts:45-64` single lazy-load promise.
    #[tokio::test]
    async fn lazy_oauth_loads_once_across_operations() {
        let loads = Arc::new(AtomicUsize::new(0));
        let loader_calls = loads.clone();
        let oauth = lazy_oauth(
            "OAuth".to_owned(),
            Some(true),
            None,
            Arc::new(move || {
                loader_calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Ok(OAuthAuth {
                        name: "loaded".to_owned(),
                        is_subscription: Some(true),
                        login_label: None,
                        login: Arc::new(|_| Box::pin(async { Err(AuthError::new("unused")) })),
                        refresh: Arc::new(|credential, _| Box::pin(async move { Ok(credential) })),
                        to_auth: Arc::new(|credential| {
                            Box::pin(async move {
                                Ok(ModelAuth {
                                    api_key: Some(credential.access),
                                    ..ModelAuth::default()
                                })
                            })
                        }),
                    })
                })
            }),
        );
        assert_eq!(
            (oauth.to_auth)(oauth_credential())
                .await
                .expect("auth")
                .api_key
                .as_deref(),
            Some("access")
        );
        (oauth.refresh)(oauth_credential(), AbortController::new().signal())
            .await
            .expect("refresh");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }
}
