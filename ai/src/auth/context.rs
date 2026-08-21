//! Default auth context ⇐ pi `src/auth/context.ts`.

use super::types::{AuthContext, AuthFuture};
use std::path::PathBuf;

#[allow(deprecated)]
fn home_dir() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultProviderAuthContext;

impl AuthContext for DefaultProviderAuthContext {
    fn env(&self, name: String) -> AuthFuture<Option<String>> {
        Box::pin(async move {
            Ok(std::env::var(name).ok().filter(|value| {
                !crate::utils::error_body::trim_javascript_whitespace(value).is_empty()
            }))
        })
    }

    fn file_exists(&self, path: String) -> AuthFuture<bool> {
        Box::pin(async move {
            let resolved = if let Some(rest) = path.strip_prefix('~') {
                let home = home_dir();
                PathBuf::from(format!("{}{rest}", home.to_string_lossy()))
            } else {
                PathBuf::from(path)
            };
            Ok(std::fs::metadata(resolved).is_ok())
        })
    }
}

pub fn default_provider_auth_context() -> DefaultProviderAuthContext {
    DefaultProviderAuthContext
}
