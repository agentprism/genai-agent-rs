//! Error type for the token-source boundary.
//!
//! Note the division of labor: *stream* failures (transport, handshake, protocol,
//! cancellation) are never returned as `Result` errors — they are reported
//! **in-band** as a terminal [`AssistantMessageEvent::Error`] on the returned
//! stream, exactly as the [`rust_genai_agent::StreamFn`] contract requires. The
//! only fallible boundary that surfaces a `Result` is the [`crate::TokenSource`],
//! and even that error is converted to an in-band terminal event by
//! [`crate::CodexStreamFn`]. This enum therefore exists for token-source
//! implementations and callers that want a typed error.
//!
//! [`AssistantMessageEvent::Error`]: rust_genai_agent::AssistantMessageEvent::Error

/// An error obtaining a fresh Codex bearer + account id from a [`crate::TokenSource`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodexError {
    /// The token source failed to produce a usable bearer / account id.
    #[error("codex token source failed: {0}")]
    Token(String),
}

impl CodexError {
    /// Build a [`CodexError::Token`] from any displayable value.
    pub fn token(message: impl std::fmt::Display) -> Self {
        Self::Token(message.to_string())
    }
}

/// Convenience alias for token-source results.
pub type Result<T> = std::result::Result<T, CodexError>;
