//! Provider auth resolution ⇐ pi `src/auth/resolve.ts`.

use super::types::*;
use crate::types::{AbortSignal, ProviderEnv};
use crate::utils::abort::{
    AbortController, AbortReason, abort_reason, operation_signal, race_with_abort_signal,
};
use crate::utils::abort_signals::combine_abort_signals;
use futures::future::BoxFuture;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelsErrorCode {
    ModelSource,
    ModelValidation,
    Provider,
    Stream,
    Auth,
    OAuth,
}

impl ModelsErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelSource => "model_source",
            Self::ModelValidation => "model_validation",
            Self::Provider => "provider",
            Self::Stream => "stream",
            Self::Auth => "auth",
            Self::OAuth => "oauth",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsError {
    pub code: ModelsErrorCode,
    pub message: String,
}

impl ModelsError {
    pub fn new(
        code: ModelsErrorCode,
        message: impl Into<String>,
        cause: Option<&dyn fmt::Display>,
    ) -> Self {
        let message = message.into();
        let message = cause.map_or(message.clone(), |cause| with_cause_detail(&message, cause));
        Self { code, message }
    }
}

impl fmt::Display for ModelsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ModelsError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveProviderAuthError {
    Abort(AbortReason),
    Models(ModelsError),
}

impl fmt::Display for ResolveProviderAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Abort(error) => error.fmt(formatter),
            Self::Models(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ResolveProviderAuthError {}

fn with_cause_detail(message: &str, cause: &dyn fmt::Display) -> String {
    let detail = cause.to_string();
    let detail = detail.trim();
    if detail.is_empty() || message.contains(detail) {
        message.to_owned()
    } else {
        format!("{message}: {detail}")
    }
}

#[derive(Clone, Default)]
pub struct AuthResolutionOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    pub min_oauth_validity_ms: Option<f64>,
    pub signal: Option<Arc<dyn AbortSignal>>,
}

#[derive(Clone)]
struct OverlayAuthContext {
    base: Arc<dyn AuthContext>,
    env: ProviderEnv,
}

impl AuthContext for OverlayAuthContext {
    fn env(&self, name: String) -> AuthFuture<Option<String>> {
        let scoped = self
            .env
            .get(&name)
            .filter(|value| !value.is_empty())
            .cloned();
        let base = self.base.clone();
        Box::pin(async move {
            if scoped.is_some() {
                Ok(scoped)
            } else {
                base.env(name).await
            }
        })
    }

    fn file_exists(&self, path: String) -> AuthFuture<bool> {
        self.base.file_exists(path)
    }
}

pub type ResolveAuthFuture =
    BoxFuture<'static, Result<Option<AuthResult>, ResolveProviderAuthError>>;

pub fn resolve_provider_auth(
    provider_id: impl Into<String>,
    auth: ProviderAuth,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
    overrides: AuthResolutionOverrides,
) -> ResolveAuthFuture {
    let provider_id = provider_id.into();
    let signal = operation_signal(overrides.signal.clone());
    let race_signal = signal.clone();
    Box::pin(async move {
        let operation = resolve_provider_auth_with_signal(
            provider_id,
            auth,
            credentials,
            auth_context,
            overrides,
            signal,
        );
        race_with_abort_signal(operation, race_signal)
            .await
            .map_err(ResolveProviderAuthError::Abort)?
    })
}

async fn resolve_provider_auth_with_signal(
    provider_id: String,
    auth: ProviderAuth,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
    overrides: AuthResolutionOverrides,
    signal: Arc<dyn AbortSignal>,
) -> Result<Option<AuthResult>, ResolveProviderAuthError> {
    if signal.is_aborted() {
        return Err(ResolveProviderAuthError::Abort(abort_reason(
            signal.as_ref(),
        )));
    }
    let request_context: Arc<dyn AuthContext> =
        overrides.env.clone().map_or(auth_context.clone(), |env| {
            Arc::new(OverlayAuthContext {
                base: auth_context,
                env,
            })
        });
    if let (Some(key), Some(api_key)) = (overrides.api_key.clone(), auth.api_key.clone()) {
        return resolve_api_key(
            request_context,
            api_key,
            &provider_id,
            Some(ApiKeyCredential {
                kind: ApiKeyCredentialType::ApiKey,
                key: Some(key),
                env: overrides.env.clone(),
            }),
            signal,
        )
        .await;
    }

    let stored = credentials
        .read(
            provider_id.clone(),
            AuthOperationOptions {
                signal: Some(signal.clone()),
            },
        )
        .await
        .map_err(|error| {
            ResolveProviderAuthError::Models(ModelsError::new(
                ModelsErrorCode::Auth,
                format!("Credential store read failed for {provider_id}"),
                Some(&error),
            ))
        })?;
    if let Some(stored) = stored {
        return match (stored, auth.oauth, auth.api_key) {
            (Credential::OAuth(stored), Some(oauth), _) => {
                resolve_stored_oauth(
                    credentials,
                    provider_id,
                    oauth,
                    stored,
                    signal,
                    overrides.min_oauth_validity_ms,
                )
                .await
            }
            (Credential::ApiKey(mut stored), _, Some(api_key)) => {
                if let Some(env) = overrides.env {
                    stored.env.get_or_insert_with(ProviderEnv::new).extend(env);
                }
                resolve_api_key(request_context, api_key, &provider_id, Some(stored), signal).await
            }
            _ => Ok(None),
        };
    }
    if let Some(api_key) = auth.api_key {
        resolve_api_key(request_context, api_key, &provider_id, None, signal).await
    } else {
        Ok(None)
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1_000.0
}

async fn resolve_stored_oauth(
    credentials: Arc<dyn CredentialStore>,
    provider_id: String,
    oauth: OAuthAuth,
    stored: OAuthCredential,
    signal: Arc<dyn AbortSignal>,
    min_oauth_validity_ms: Option<f64>,
) -> Result<Option<AuthResult>, ResolveProviderAuthError> {
    const DEFAULT_MINIMUM: f64 = 5.0 * 60.0 * 1_000.0;
    const REFRESH_TIMEOUT_MS: u64 = 15_000;
    let minimum = match min_oauth_validity_ms {
        Some(value) if value.is_nan() => f64::NAN,
        Some(value) => DEFAULT_MINIMUM.max(value),
        None => DEFAULT_MINIMUM,
    };
    let expires_soon = |credential: &OAuthCredential| now_ms() + minimum >= credential.expires;
    let mut credential = stored;
    if expires_soon(&credential) {
        let callback_provider = provider_id.clone();
        let callback_oauth = oauth.clone();
        let callback_signal = signal.clone();
        let post = credentials
            .modify(
                provider_id.clone(),
                Box::new(move |current| {
                    Box::pin(async move {
                        let Some(Credential::OAuth(current)) = current else {
                            return Ok(None);
                        };
                        if now_ms() + minimum < current.expires {
                            return Ok(None);
                        }
                        let timeout = AbortController::new();
                        let timeout_task = {
                            let timeout = timeout.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(REFRESH_TIMEOUT_MS)).await;
                                timeout.abort(AbortReason::new(
                                    "TimeoutError",
                                    "The operation was aborted due to timeout",
                                ));
                            })
                        };
                        let mut combined = combine_abort_signals(&[
                            Some(callback_signal.clone()),
                            Some(timeout.signal()),
                        ]);
                        let refresh_signal = combined.signal.clone().expect("combined signal");
                        let refreshed = ((callback_oauth.refresh)(current, refresh_signal).await)
                            .map(Credential::OAuth)
                            .map_err(|error| {
                                AuthError::coded(
                                    "oauth",
                                    with_cause_detail(
                                        &format!("OAuth refresh failed for {callback_provider}"),
                                        &error,
                                    ),
                                )
                            });
                        timeout_task.abort();
                        combined.cleanup();
                        refreshed.map(Some)
                    })
                }),
                AuthOperationOptions {
                    signal: Some(signal.clone()),
                },
            )
            .await
            .map_err(|error| {
                if error.code.as_deref() == Some("oauth") {
                    ResolveProviderAuthError::Models(ModelsError::new(
                        ModelsErrorCode::OAuth,
                        error.message,
                        None,
                    ))
                } else {
                    ResolveProviderAuthError::Models(ModelsError::new(
                        ModelsErrorCode::Auth,
                        format!("Credential store modify failed for {provider_id}"),
                        Some(&error),
                    ))
                }
            })?;
        let Some(Credential::OAuth(post)) = post else {
            return Ok(None);
        };
        credential = post;
        if min_oauth_validity_ms.is_some() && expires_soon(&credential) {
            return Err(ResolveProviderAuthError::Models(ModelsError::new(
                ModelsErrorCode::OAuth,
                format!("OAuth refresh returned a token that expires too soon for {provider_id}"),
                None,
            )));
        }
    }

    let auth = (oauth.to_auth)(credential).await.map_err(|error| {
        ResolveProviderAuthError::Models(ModelsError::new(
            ModelsErrorCode::OAuth,
            format!("OAuth auth derivation failed for {provider_id}"),
            Some(&error),
        ))
    })?;
    Ok(Some(AuthResult {
        auth,
        env: None,
        source: Some("OAuth".to_owned()),
    }))
}

async fn resolve_api_key(
    auth_context: Arc<dyn AuthContext>,
    api_key: ApiKeyAuth,
    provider_id: &str,
    credential: Option<ApiKeyCredential>,
    signal: Arc<dyn AbortSignal>,
) -> Result<Option<AuthResult>, ResolveProviderAuthError> {
    (api_key.resolve)(ApiKeyResolveInput {
        ctx: auth_context,
        credential,
        signal,
    })
    .await
    .map_err(|error| {
        ResolveProviderAuthError::Models(ModelsError::new(
            ModelsErrorCode::Auth,
            format!("API key auth failed for provider {provider_id}"),
            Some(&error),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::context::default_provider_auth_context;
    use crate::auth::credential_store::InMemoryCredentialStore;
    use crate::auth::helpers::env_api_key_auth;
    use serde_json::Map;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn api_credential(key: &str) -> Credential {
        Credential::ApiKey(ApiKeyCredential {
            kind: ApiKeyCredentialType::ApiKey,
            key: Some(key.to_owned()),
            env: None,
        })
    }

    fn oauth_credential(access: &str, expires: f64) -> Credential {
        Credential::OAuth(OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "refresh".to_owned(),
            access: access.to_owned(),
            expires,
            extra: Map::new(),
        })
    }

    async fn put(store: &InMemoryCredentialStore, provider: &str, value: Credential) {
        store
            .modify(
                provider.to_owned(),
                Box::new(move |_| Box::pin(async move { Ok(Some(value)) })),
                AuthOperationOptions::default(),
            )
            .await
            .expect("put");
    }

    /// Pins pi `src/auth/resolve.ts:60-103` precedence and stored ownership.
    #[tokio::test]
    async fn explicit_override_wins_and_mismatched_stored_type_blocks_ambient() {
        let store = InMemoryCredentialStore::default();
        put(&store, "provider", api_credential("stored")).await;
        let auth = ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Provider key",
                vec!["__PI_RESOLVE_KEY__".to_owned()],
            )),
            oauth: None,
        };
        let result = resolve_provider_auth(
            "provider",
            auth.clone(),
            Arc::new(store.clone()),
            Arc::new(default_provider_auth_context()),
            AuthResolutionOverrides {
                api_key: Some("override".to_owned()),
                ..AuthResolutionOverrides::default()
            },
        )
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(result.auth.api_key.as_deref(), Some("override"));

        put(&store, "provider", oauth_credential("oauth", f64::MAX)).await;
        let result = resolve_provider_auth(
            "provider",
            auth,
            Arc::new(store),
            Arc::new(default_provider_auth_context()),
            AuthResolutionOverrides {
                env: Some(ProviderEnv::from([(
                    "__PI_RESOLVE_KEY__".to_owned(),
                    "ambient".to_owned(),
                )])),
                ..AuthResolutionOverrides::default()
            },
        )
        .await
        .expect("resolve");
        assert_eq!(result, None);
    }

    /// Pins pi `src/auth/resolve.ts:127-174` double-checked refresh locking.
    #[tokio::test]
    async fn concurrent_expired_oauth_refreshes_once() {
        let store = InMemoryCredentialStore::default();
        put(&store, "provider", oauth_credential("expired", 0.0)).await;
        let refreshes = Arc::new(AtomicUsize::new(0));
        let refresh_calls = refreshes.clone();
        let oauth = OAuthAuth {
            name: "OAuth".to_owned(),
            is_subscription: Some(true),
            login_label: None,
            login: Arc::new(|_| Box::pin(async { Err(AuthError::new("unused")) })),
            refresh: Arc::new(move |mut credential, _| {
                refresh_calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    credential.access = "fresh".to_owned();
                    credential.expires = f64::MAX;
                    Ok(credential)
                })
            }),
            to_auth: Arc::new(|credential| {
                Box::pin(async move {
                    Ok(ModelAuth {
                        api_key: Some(credential.access),
                        ..ModelAuth::default()
                    })
                })
            }),
        };
        let resolve = || {
            resolve_provider_auth(
                "provider",
                ProviderAuth {
                    api_key: None,
                    oauth: Some(oauth.clone()),
                },
                Arc::new(store.clone()),
                Arc::new(default_provider_auth_context()),
                AuthResolutionOverrides::default(),
            )
        };
        let (first, second) = tokio::join!(resolve(), resolve());
        assert_eq!(
            first.expect("first").expect("auth").auth.api_key.as_deref(),
            Some("fresh")
        );
        assert_eq!(
            second
                .expect("second")
                .expect("auth")
                .auth
                .api_key
                .as_deref(),
            Some("fresh")
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    }
}
