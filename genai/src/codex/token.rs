//! The token-source abstraction: a fresh Codex bearer + `chatgpt-account-id`.
//!
//! [`CodexStreamFn`](crate::codex::CodexStreamFn) resolves a fresh token **before every
//! request** (pi refreshes per request too). The token carries two things pi
//! reads on every Codex call:
//!
//! - the OAuth **bearer** access token (`Authorization: Bearer …`), and
//! - the **`chatgpt-account-id`**, which pi decodes from the token's JWT claim
//!   `https://api.openai.com/auth.chatgpt_account_id`
//!   (openai-codex-responses.ts:1579-1590).
//!
//! This crate stays decoupled from *how* the token is obtained. Provide any of:
//!
//! - a plain async closure `|| async { Ok(CodexToken { .. }) }` (blanket impl),
//! - [`StaticTokenSource`] for a fixed token (tests / short-lived tools),
//! - or, with the `codex-auth-resolver` feature, [`ResolverTokenSource`], which
//!   wraps `genai::auth`'s `CodexTokenResolver` (fresh bearer, expiry-aware
//!   refresh and persist, with the double-refresh race fix) and derives the
//!   account id from the bearer JWT via that module's `jwt`.

use async_trait::async_trait;
use std::future::Future;

#[cfg(feature = "codex-auth-resolver")]
use crate::codex::error::CodexError;
use crate::codex::error::Result;

/// A resolved Codex credential for one request.
///
/// Both fields are secrets-adjacent: `bearer` is the OAuth access token and
/// `account_id` identifies the ChatGPT account. `Debug` redacts the bearer.
#[derive(Clone)]
pub struct CodexToken {
    /// OAuth bearer access token for `Authorization: Bearer …`.
    pub bearer: String,
    /// ChatGPT account id for the `chatgpt-account-id` header.
    pub account_id: String,
}

impl std::fmt::Debug for CodexToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexToken")
            .field("bearer", &"<redacted>")
            .field("account_id", &self.account_id)
            .finish()
    }
}

impl CodexToken {
    /// Construct a token from a bearer and account id.
    pub fn new(bearer: impl Into<String>, account_id: impl Into<String>) -> Self {
        Self {
            bearer: bearer.into(),
            account_id: account_id.into(),
        }
    }
}

/// Source of a fresh Codex bearer + account id, resolved once per request.
///
/// Implementations must be cheap to call repeatedly; a resolver that refreshes
/// an OAuth token should coordinate its own refresh (`genai::auth`'s
/// `CodexTokenResolver` does). Returning an error causes
/// [`CodexStreamFn`](crate::codex::CodexStreamFn) to emit an in-band terminal error
/// event (never a panic or a thrown error).
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// Resolve a fresh bearer + account id for the next request.
    async fn fetch(&self) -> Result<CodexToken>;
}

/// Any `Fn() -> Future<Output = Result<CodexToken>>` is a [`TokenSource`].
///
/// This is the `dyn Fn -> Future<(bearer, account_id)>` shape called out in the
/// design: pass a closure that returns a fresh token.
#[async_trait]
impl<F, Fut> TokenSource for F
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = Result<CodexToken>> + Send,
{
    async fn fetch(&self) -> Result<CodexToken> {
        (self)().await
    }
}

/// A fixed, non-refreshing token source.
///
/// Handy for tests (a stub token — no real OAuth) and for short-lived tools that
/// already hold a fresh bearer. Production agents should prefer a refreshing
/// source ([`ResolverTokenSource`] with the `auth-resolver` feature).
#[derive(Clone, Debug)]
pub struct StaticTokenSource {
    token: CodexToken,
}

impl StaticTokenSource {
    /// Wrap a fixed bearer + account id.
    pub fn new(bearer: impl Into<String>, account_id: impl Into<String>) -> Self {
        Self {
            token: CodexToken::new(bearer, account_id),
        }
    }
}

#[async_trait]
impl TokenSource for StaticTokenSource {
    async fn fetch(&self) -> Result<CodexToken> {
        Ok(self.token.clone())
    }
}

/// Adapter turning `genai::auth`'s `CodexTokenResolver` into a [`TokenSource`]
/// (feature `codex-auth-resolver`).
///
/// On each `fetch` it calls `CodexTokenResolver::resolve()` — which loads the
/// stored credential, refreshes it if expired (serialized in-process +
/// cross-process, persisting the fresh token), and returns the bearer — then
/// derives the `chatgpt-account-id` from the bearer JWT with
/// `genai::auth::jwt::extract_chatgpt_account_id`, mirroring pi's
/// `extractAccountId(apiKey)` (openai-codex-responses.ts:1579-1590).
#[cfg(feature = "codex-auth-resolver")]
pub struct ResolverTokenSource {
    resolver: std::sync::Arc<genai::auth::genai_integration::CodexTokenResolver>,
}

#[cfg(feature = "codex-auth-resolver")]
impl ResolverTokenSource {
    /// Wrap a shared [`CodexTokenResolver`](genai::auth::genai_integration::CodexTokenResolver).
    pub fn new(
        resolver: std::sync::Arc<genai::auth::genai_integration::CodexTokenResolver>,
    ) -> Self {
        Self { resolver }
    }
}

#[cfg(feature = "codex-auth-resolver")]
#[async_trait]
impl TokenSource for ResolverTokenSource {
    async fn fetch(&self) -> Result<CodexToken> {
        let bearer = self.resolver.resolve().await.map_err(CodexError::token)?;
        let account_id = genai::auth::jwt::extract_chatgpt_account_id(&bearer)
            .ok_or_else(|| CodexError::token("access token has no chatgpt_account_id claim"))?;
        Ok(CodexToken { bearer, account_id })
    }
}
