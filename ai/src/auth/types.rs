//! Authentication data contracts ⇐ pi `src/auth/types.ts`.

use crate::types::{AbortSignal, ProviderEnv, ProviderHeaders};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthError {
    pub name: String,
    pub message: String,
    pub code: Option<String>,
}

impl AuthError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            name: "Error".to_owned(),
            message: message.into(),
            code: None,
        }
    }

    pub fn abort(reason: crate::utils::abort::AbortReason) -> Self {
        Self {
            name: reason.name,
            message: reason.message,
            code: None,
        }
    }

    pub fn coded(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: "ModelsError".to_owned(),
            message: message.into(),
            code: Some(code.into()),
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuthError {}

pub type AuthFuture<T> = BoxFuture<'static, Result<T, AuthError>>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(rename = "type")]
    pub kind: ApiKeyCredentialType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiKeyCredentialType {
    #[default]
    #[serde(rename = "api_key")]
    ApiKey,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    #[serde(rename = "type")]
    pub kind: OAuthCredentialType,
    pub refresh: String,
    pub access: String,
    pub expires: f64,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuthCredentialType {
    #[default]
    #[serde(rename = "oauth")]
    OAuth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Credential {
    ApiKey(ApiKeyCredential),
    OAuth(OAuthCredential),
}

impl Credential {
    pub fn auth_type(&self) -> AuthType {
        match self {
            Self::ApiKey(_) => AuthType::ApiKey,
            Self::OAuth(_) => AuthType::OAuth,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthType {
    #[serde(rename = "api_key")]
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInfo {
    pub provider_id: String,
    pub kind: AuthType,
}

#[derive(Clone, Default)]
pub struct AuthOperationOptions {
    pub signal: Option<Arc<dyn AbortSignal>>,
}

pub type CredentialModify =
    Box<dyn FnOnce(Option<Credential>) -> AuthFuture<Option<Credential>> + Send + 'static>;

pub trait CredentialStore: Send + Sync {
    fn read(
        &self,
        provider_id: String,
        options: AuthOperationOptions,
    ) -> AuthFuture<Option<Credential>>;
    fn list(&self, options: AuthOperationOptions) -> AuthFuture<Vec<CredentialInfo>>;
    fn modify(
        &self,
        provider_id: String,
        modify: CredentialModify,
        options: AuthOperationOptions,
    ) -> AuthFuture<Option<Credential>>;
    fn delete(&self, provider_id: String, options: AuthOperationOptions) -> AuthFuture<()>;
}

pub trait AuthContext: Send + Sync {
    fn env(&self, name: String) -> AuthFuture<Option<String>>;
    fn file_exists(&self, path: String) -> AuthFuture<bool>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthResult {
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCheck {
    pub source: Option<String>,
    pub kind: AuthType,
}

#[derive(Clone)]
pub enum AuthPrompt {
    Text {
        message: String,
        placeholder: Option<String>,
        signal: Option<Arc<dyn AbortSignal>>,
    },
    Secret {
        message: String,
        placeholder: Option<String>,
        signal: Option<Arc<dyn AbortSignal>>,
    },
    Select {
        message: String,
        options: Vec<AuthSelectOption>,
        signal: Option<Arc<dyn AbortSignal>>,
    },
    ManualCode {
        message: String,
        placeholder: Option<String>,
        signal: Option<Arc<dyn AbortSignal>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSelectOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthEvent {
    Info {
        message: String,
        links: Option<Vec<AuthInfoLink>>,
    },
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        interval_seconds: Option<f64>,
        expires_in_seconds: Option<f64>,
    },
    Progress {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthInfoLink {
    pub url: String,
    pub label: Option<String>,
}

pub trait AuthInteraction: Send + Sync {
    fn signal(&self) -> Option<Arc<dyn AbortSignal>>;
    fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<String>;
    fn notify(&self, event: AuthEvent);
}

#[derive(Clone)]
pub struct ProviderAuthInteraction {
    pub interaction: Arc<dyn AuthInteraction>,
    pub signal: Arc<dyn AbortSignal>,
}

#[derive(Clone)]
pub struct ApiKeyResolveInput {
    pub ctx: Arc<dyn AuthContext>,
    pub credential: Option<ApiKeyCredential>,
    pub signal: Arc<dyn AbortSignal>,
}

pub type ApiKeyLogin =
    Arc<dyn Fn(ProviderAuthInteraction) -> AuthFuture<ApiKeyCredential> + Send + Sync>;
pub type ApiKeyCheck =
    Arc<dyn Fn(ApiKeyResolveInput) -> AuthFuture<Option<AuthCheck>> + Send + Sync>;
pub type ApiKeyResolve =
    Arc<dyn Fn(ApiKeyResolveInput) -> AuthFuture<Option<AuthResult>> + Send + Sync>;

#[derive(Clone)]
pub struct ApiKeyAuth {
    pub name: String,
    pub login: Option<ApiKeyLogin>,
    pub check: Option<ApiKeyCheck>,
    pub resolve: ApiKeyResolve,
}

pub type OAuthLogin =
    Arc<dyn Fn(ProviderAuthInteraction) -> AuthFuture<OAuthCredential> + Send + Sync>;
pub type OAuthRefresh =
    Arc<dyn Fn(OAuthCredential, Arc<dyn AbortSignal>) -> AuthFuture<OAuthCredential> + Send + Sync>;
pub type OAuthToAuth = Arc<dyn Fn(OAuthCredential) -> AuthFuture<ModelAuth> + Send + Sync>;

#[derive(Clone)]
pub struct OAuthAuth {
    pub name: String,
    pub is_subscription: Option<bool>,
    pub login_label: Option<String>,
    pub login: OAuthLogin,
    pub refresh: OAuthRefresh,
    pub to_auth: OAuthToAuth,
}

#[derive(Clone, Default)]
pub struct ProviderAuth {
    pub api_key: Option<ApiKeyAuth>,
    pub oauth: Option<OAuthAuth>,
}
