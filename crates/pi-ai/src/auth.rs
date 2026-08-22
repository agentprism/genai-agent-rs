//! Secret-safe OAuth credential values from Architecture v2 part 2 §6.6.

use crate::{ExtensionId, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::fmt;
use url::Url;

/// Secret UTF-8 data that redacts `Debug` and intentionally does not implement
/// `Serialize` (Architecture v2 part 1 §3.8 and part 2 §6.6).
#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps a secret string.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Explicitly exposes the secret to authentication code.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and exposes its secret value.
    pub fn into_secret(self) -> String {
        self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

/// Canonical in-memory OAuth credential (Architecture v2 part 2 §6.6).
///
/// This type intentionally has no general-purpose serde implementation. A
/// credential store must choose an explicit protected persistence format.
#[derive(Clone, Eq, PartialEq)]
pub struct OAuthCredential {
    /// Current access token.
    pub access: SecretString,
    /// Refresh token used for rotation.
    pub refresh: SecretString,
    /// Absolute access-token expiry in Unix milliseconds.
    pub expires_at: Timestamp,
    /// Typed provider-specific noncanonical fields.
    pub extra: ProviderOAuthExtra,
}

impl fmt::Debug for OAuthCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthCredential")
            .field("access", &self.access)
            .field("refresh", &self.refresh)
            .field("expires_at", &self.expires_at)
            .field("extra", &"[REDACTED CREDENTIAL EXTRA]")
            .finish()
    }
}

/// Typed provider-owned OAuth fields (Architecture v2 part 2 §6.6).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "provider", content = "value")]
pub enum ProviderOAuthExtra {
    /// No provider-specific fields.
    None,
    /// Radius gateway selection.
    Radius {
        /// Credential-specific gateway URL.
        gateway_url: Url,
        /// Optional Radius organization identifier.
        organization_id: Option<String>,
    },
    /// GitHub Copilot endpoint and account identity.
    GitHubCopilot {
        /// Credential-specific API endpoint.
        api_endpoint: Url,
        /// Optional account identifier.
        account_id: Option<String>,
    },
    /// OpenAI Codex account identity.
    OpenAiCodex {
        /// Required account identifier.
        account_id: String,
    },
    /// Third-party typed credential data.
    Custom {
        /// Extension schema owner.
        schema: ExtensionId,
        /// Extension schema version.
        schema_version: u32,
        /// Exact custom JSON value.
        value: Box<RawValue>,
    },
}

impl PartialEq for ProviderOAuthExtra {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) => true,
            (
                Self::Radius {
                    gateway_url: left_url,
                    organization_id: left_organization,
                },
                Self::Radius {
                    gateway_url: right_url,
                    organization_id: right_organization,
                },
            ) => left_url == right_url && left_organization == right_organization,
            (
                Self::GitHubCopilot {
                    api_endpoint: left_endpoint,
                    account_id: left_account,
                },
                Self::GitHubCopilot {
                    api_endpoint: right_endpoint,
                    account_id: right_account,
                },
            ) => left_endpoint == right_endpoint && left_account == right_account,
            (
                Self::OpenAiCodex {
                    account_id: left_account,
                },
                Self::OpenAiCodex {
                    account_id: right_account,
                },
            ) => left_account == right_account,
            (
                Self::Custom {
                    schema: left_schema,
                    schema_version: left_version,
                    value: left_value,
                },
                Self::Custom {
                    schema: right_schema,
                    schema_version: right_version,
                    value: right_value,
                },
            ) => {
                left_schema == right_schema
                    && left_version == right_version
                    && left_value.get() == right_value.get()
            }
            _ => false,
        }
    }
}

impl Eq for ProviderOAuthExtra {}
