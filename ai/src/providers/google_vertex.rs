use super::google_vertex_models::GOOGLE_VERTEX_MODELS;
use crate::api::google_vertex::google_vertex_api;
use crate::auth::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyCredentialType, AuthError, AuthEvent, AuthFuture,
    AuthInfoLink, AuthPrompt, AuthResult, AuthSelectOption, ModelAuth, ProviderAuth,
    ProviderAuthInteraction,
};
use crate::models::{CreateProviderOptions, ProviderApi, ProviderRef, create_provider};
use crate::types::ProviderEnv;
use crate::utils::abort::abort_reason;
use std::sync::Arc;

const VERTEX_ADC_PATH: &str = "~/.config/gcloud/application_default_credentials.json";

fn ensure_not_aborted(signal: &Arc<dyn crate::types::AbortSignal>) -> Result<(), AuthError> {
    if signal.is_aborted() {
        Err(AuthError::abort(abort_reason(signal.as_ref())))
    } else {
        Ok(())
    }
}

fn vertex_login(interaction: ProviderAuthInteraction) -> AuthFuture<ApiKeyCredential> {
    Box::pin(async move {
        ensure_not_aborted(&interaction.signal)?;
        let method = interaction
            .interaction
            .prompt(AuthPrompt::Select {
                message: "Select Google Vertex AI authentication method:".to_owned(),
                options: vec![
                    AuthSelectOption {
                        id: "api-key".to_owned(),
                        label: "Google Cloud API key".to_owned(),
                        description: None,
                    },
                    AuthSelectOption {
                        id: "adc".to_owned(),
                        label: "Application Default Credentials".to_owned(),
                        description: None,
                    },
                    AuthSelectOption {
                        id: "service-account".to_owned(),
                        label: "Service account credentials file".to_owned(),
                        description: None,
                    },
                ],
                signal: None,
            })
            .await?;
        ensure_not_aborted(&interaction.signal)?;
        if method == "api-key" {
            let key = interaction
                .interaction
                .prompt(AuthPrompt::Secret {
                    message: "Enter Google Cloud API key".to_owned(),
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
        if !matches!(method.as_str(), "adc" | "service-account") {
            return Err(AuthError::new(format!(
                "Unknown Google Vertex AI auth method: {method}"
            )));
        }
        interaction.interaction.notify(AuthEvent::Info {
            message: if method == "adc" {
                "Run `gcloud auth application-default login`, then provide the project and location."
                    .to_owned()
            } else {
                "Provide a service account credentials file, project, and location.".to_owned()
            },
            links: Some(vec![AuthInfoLink {
                label: Some("Application Default Credentials".to_owned()),
                url: "https://cloud.google.com/docs/authentication/provide-credentials-adc"
                    .to_owned(),
            }]),
        });
        let project = interaction
            .interaction
            .prompt(AuthPrompt::Text {
                message: "Enter Google Cloud project ID".to_owned(),
                placeholder: None,
                signal: None,
            })
            .await?;
        let location = interaction
            .interaction
            .prompt(AuthPrompt::Text {
                message: "Enter Google Cloud location".to_owned(),
                placeholder: None,
                signal: None,
            })
            .await?;
        let credentials_path = if method == "service-account" {
            Some(
                interaction
                    .interaction
                    .prompt(AuthPrompt::Text {
                        message: "Enter service account credentials file path".to_owned(),
                        placeholder: None,
                        signal: None,
                    })
                    .await?,
            )
        } else {
            None
        };
        let mut env = ProviderEnv::from([
            ("GOOGLE_CLOUD_PROJECT".to_owned(), project),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), location),
        ]);
        if let Some(path) = credentials_path.filter(|path| !path.is_empty()) {
            env.insert("GOOGLE_APPLICATION_CREDENTIALS".to_owned(), path);
        }
        Ok(ApiKeyCredential {
            kind: ApiKeyCredentialType::ApiKey,
            key: None,
            env: Some(env),
        })
    })
}

async fn read_env(
    input: &crate::auth::types::ApiKeyResolveInput,
    name: &str,
) -> Result<Option<String>, AuthError> {
    ensure_not_aborted(&input.signal)?;
    let value = input.ctx.env(name.to_owned()).await?;
    ensure_not_aborted(&input.signal)?;
    Ok(value)
}

fn vertex_auth() -> ApiKeyAuth {
    ApiKeyAuth {
        name: "Google Cloud credentials".to_owned(),
        login: Some(Arc::new(vertex_login)),
        check: None,
        resolve: Arc::new(|input| {
            Box::pin(async move {
                let stored_key = input
                    .credential
                    .as_ref()
                    .and_then(|credential| credential.key.clone());
                let key = if stored_key.is_some() {
                    stored_key.clone()
                } else {
                    read_env(&input, "GOOGLE_CLOUD_API_KEY").await?
                };
                if let Some(key) = key.filter(|key| !key.is_empty()) {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth {
                            api_key: Some(key),
                            ..ModelAuth::default()
                        },
                        env: None,
                        source: Some(if stored_key.is_some() {
                            "stored credential".to_owned()
                        } else {
                            "GOOGLE_CLOUD_API_KEY".to_owned()
                        }),
                    }));
                }
                let stored_env = input
                    .credential
                    .as_ref()
                    .and_then(|credential| credential.env.as_ref());
                let adc_path = if let Some(path) =
                    stored_env.and_then(|env| env.get("GOOGLE_APPLICATION_CREDENTIALS").cloned())
                {
                    Some(path)
                } else {
                    read_env(&input, "GOOGLE_APPLICATION_CREDENTIALS").await?
                };
                ensure_not_aborted(&input.signal)?;
                let has_credentials = input
                    .ctx
                    .file_exists(adc_path.unwrap_or_else(|| VERTEX_ADC_PATH.to_owned()))
                    .await?;
                ensure_not_aborted(&input.signal)?;
                let project = if let Some(project) =
                    stored_env.and_then(|env| env.get("GOOGLE_CLOUD_PROJECT").cloned())
                {
                    Some(project)
                } else if let Some(project) = read_env(&input, "GOOGLE_CLOUD_PROJECT").await? {
                    Some(project)
                } else {
                    read_env(&input, "GCLOUD_PROJECT").await?
                };
                let location = if let Some(location) =
                    stored_env.and_then(|env| env.get("GOOGLE_CLOUD_LOCATION").cloned())
                {
                    Some(location)
                } else {
                    read_env(&input, "GOOGLE_CLOUD_LOCATION").await?
                };
                if has_credentials
                    && project.as_deref().is_some_and(|value| !value.is_empty())
                    && location.as_deref().is_some_and(|value| !value.is_empty())
                {
                    return Ok(Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: input
                            .credential
                            .as_ref()
                            .and_then(|credential| credential.env.clone()),
                        source: Some(if input.credential.is_some() {
                            "stored credential".to_owned()
                        } else {
                            "gcloud application default credentials".to_owned()
                        }),
                    }));
                }
                Ok(None)
            })
        }),
    }
}

pub fn google_vertex_provider() -> ProviderRef {
    create_provider(CreateProviderOptions {
        id: "google-vertex".to_owned(),
        name: Some("Google Vertex AI".to_owned()),
        base_url: None,
        headers: None,
        auth: ProviderAuth {
            api_key: Some(vertex_auth()),
            oauth: None,
        },
        models: GOOGLE_VERTEX_MODELS.values().cloned().collect(),
        fetch_models: None,
        filter_models: None,
        api: ProviderApi::Single(Arc::new(google_vertex_api())),
    })
}

#[cfg(test)]
mod tests;
