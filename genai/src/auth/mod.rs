//! `genai::auth` — OAuth login / token cache / refresh for genai.
//!
//! This module is the structural equivalent of pi-ai's `packages/ai/src/auth/`.
//! It owns OAuth login, the on-disk token cache, and refresh, so that the rest of
//! `genai-agentprism` and higher-level agents can stay auth-agnostic.
//!
//! This first release delivers the **ChatGPT Codex** (OpenAI Codex /
//! ChatGPT-subscription) OAuth flow, ported faithfully from pi-ai's
//! `auth/oauth/openai-codex.ts`, `pkce.ts`, and `device-code.ts`.
//!
//! # What it provides
//!
//! - [`OAuthCredential`] — the stored credential shape (serde-mapped to pi's
//!   on-disk `auth.json` keys) with expiry bookkeeping.
//! - [`CredentialStore`] — the storage seam, plus a default file-backed
//!   [`FileCredentialStore`] (atomic writes, `0600`, env-overridable path).
//! - [`Pkce`] — S256 PKCE generation.
//! - [`CodexAuth`] — the Codex OAuth flow: browser login, device-code login, and
//!   token refresh.
//! - Optional loopback redirect capture (feature `loopback`).
//! - Optional [`genai`] `AuthResolver` adapter (feature `genai`).
//!
//! # Browser login (application owns the browser + redirect capture)
//!
//! ```no_run
//! # async fn demo() -> genai::auth::Result<()> {
//! use genai::auth::{CodexAuth, FileCredentialStore, CredentialStore, codex};
//!
//! let auth = CodexAuth::new();
//! let store = FileCredentialStore::with_default_path()?;
//!
//! // 1) Begin: get the URL to open and the pending PKCE verifier / state.
//! let pending = auth.begin_browser_login()?;
//! println!("Open this URL in your browser:\n{}", pending.authorize_url);
//!
//! // 2) The application opens the browser and captures the redirect (or asks
//! //    the user to paste the redirect URL / code). See the `loopback` feature
//! //    for a helper. Here we assume we obtained the redirect input:
//! let redirect_input = "http://localhost:1455/auth/callback?code=THECODE&state=...";
//!
//! // 3) Complete: exchange the code for a credential and persist it.
//! let credential = auth.complete_browser_login(&pending, redirect_input).await?;
//! store.store(codex::PROVIDER_ID, &credential)?;
//! # Ok(()) }
//! ```
//!
//! # Device-code login (headless)
//!
//! ```no_run
//! # async fn demo() -> genai::auth::Result<()> {
//! use genai::auth::{CodexAuth, FileCredentialStore, CredentialStore, codex};
//!
//! let auth = CodexAuth::new();
//! let store = FileCredentialStore::with_default_path()?;
//!
//! let begin = auth.begin_device_login().await?;
//! println!("Go to {} and enter code {}", begin.verification_uri, begin.user_code);
//!
//! let credential = auth.poll_device_login(&begin).await?; // polls until authorized
//! store.store(codex::PROVIDER_ID, &credential)?;
//! # Ok(()) }
//! ```
//!
//! # genai wiring (feature = `genai`)
//!
//! ```ignore
//! use std::sync::Arc;
//! use genai::auth::{CodexAuth, FileCredentialStore, codex};
//! use genai::auth::genai_integration::codex_auth_resolver;
//! use genai::Client;
//!
//! let auth = Arc::new(CodexAuth::new());
//! let store = Arc::new(FileCredentialStore::with_default_path()?);
//! let resolver = codex_auth_resolver(auth, store, codex::PROVIDER_ID);
//!
//! let client = Client::builder().with_auth_resolver(resolver).build();
//! // The resolver refreshes the token on demand and persists the new one.
//! ```

#![forbid(unsafe_code)]

pub mod codex;
pub mod credential;
pub mod device_code;
pub mod error;
pub mod jwt;
pub mod pkce;
pub mod store;

#[cfg(feature = "loopback")]
pub mod loopback;

// The genai resolver adapter. In the standalone crate this was behind a `genai` feature (which
// pulled the `genai` path dep). Now that this module lives *inside* genai-agentprism, `genai` is
// always available, so the adapter is an unconditional part of the `auth` feature.
pub mod genai_integration;

// -- Flat re-exports of the most-used items --------------------------------

pub use codex::{
    CodexAuth, CodexConfig, DeviceLoginBegin, PendingBrowserLogin,
    PROVIDER_ID as OPENAI_CODEX_PROVIDER_ID,
};
pub use credential::{OAuthCredential, DEFAULT_EXPIRY_SKEW};
pub use error::{Error, Result};
pub use pkce::Pkce;
pub use store::{CredentialStore, FileCredentialStore};
