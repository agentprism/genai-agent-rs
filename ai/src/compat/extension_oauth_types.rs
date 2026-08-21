use crate::auth::types::AuthFuture;
use crate::types::AbortSignal;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthPrompt {
    pub message: String,
    pub placeholder: Option<String>,
    pub allow_empty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAuthInfo {
    pub url: String,
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OAuthDeviceCodeInfo {
    pub user_code: String,
    pub verification_uri: String,
    pub interval_seconds: Option<f64>,
    pub expires_in_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthSelectOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthSelectPrompt {
    pub message: String,
    pub options: Vec<OAuthSelectOption>,
}

pub trait OAuthLoginCallbacks: Send + Sync {
    fn on_auth(&self, info: OAuthAuthInfo);
    fn on_device_code(&self, info: OAuthDeviceCodeInfo);
    fn on_prompt(&self, prompt: OAuthPrompt) -> AuthFuture<String>;
    fn on_progress(&self, _message: String) {}
    fn on_manual_code_input(&self) -> Option<AuthFuture<String>> {
        None
    }
    fn on_select(&self, prompt: OAuthSelectPrompt) -> AuthFuture<Option<String>>;
    fn signal(&self) -> Option<Arc<dyn AbortSignal>> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub refresh: String,
    pub access: String,
    pub expires: f64,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pins pi `src/compat/extension-oauth-types.ts:45` and `src/auth/types.ts:24-29`.
    #[test]
    fn compatibility_credentials_preserve_extension_fields_without_a_type_tag() {
        let credentials = OAuthCredentials {
            refresh: "refresh".to_owned(),
            access: "access".to_owned(),
            expires: 42.0,
            extra: Map::from_iter([("accountId".to_owned(), json!("account"))]),
        };
        assert_eq!(
            serde_json::to_value(credentials).expect("credentials"),
            json!({
                "refresh": "refresh",
                "access": "access",
                "expires": 42.0,
                "accountId": "account"
            })
        );
    }
}
