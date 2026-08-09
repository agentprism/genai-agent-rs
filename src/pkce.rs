//! PKCE (RFC 7636) code verifier / challenge generation.
//!
//! Faithful port of pi-ai's `packages/ai/src/auth/oauth/pkce.ts`:
//! - verifier = base64url(32 random bytes)          (pkce.ts:23-25)
//! - challenge = base64url(SHA-256(verifier bytes)) (pkce.ts:28-31)
//! - base64url = standard base64 with `+`->`-`, `/`->`_`, padding stripped (pkce.ts:9-15)

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// A generated PKCE pair.
#[derive(Debug, Clone)]
pub struct Pkce {
    /// The high-entropy code verifier (kept by the client until token exchange).
    pub verifier: String,
    /// The S256 code challenge sent on the authorize request.
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh PKCE pair using the OS CSPRNG.
    ///
    /// Mirrors `generatePKCE()` (pkce.ts:21-34): 32 random bytes -> base64url verifier,
    /// SHA-256 of the verifier -> base64url challenge.
    pub fn generate() -> Result<Self> {
        let mut verifier_bytes = [0u8; 32];
        getrandom::getrandom(&mut verifier_bytes).map_err(|e| Error::Random(e.to_string()))?;
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let challenge = Self::challenge_for(&verifier);
        Ok(Self {
            verifier,
            challenge,
        })
    }

    /// Compute the S256 challenge for a given verifier.
    ///
    /// `challenge = base64url(SHA-256(verifier_utf8))` (pkce.ts:28-31).
    pub fn challenge_for(verifier: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        URL_SAFE_NO_PAD.encode(hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 Appendix B known-answer vector.
    #[test]
    fn s256_challenge_matches_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(Pkce::challenge_for(verifier), expected_challenge);
    }

    #[test]
    fn generate_produces_url_safe_no_pad_and_verifiable_challenge() {
        let pkce = Pkce::generate().expect("generate pkce");

        // base64url(32 bytes) with no padding = 43 chars.
        assert_eq!(pkce.verifier.len(), 43);
        // base64url(32-byte sha256) with no padding = 43 chars.
        assert_eq!(pkce.challenge.len(), 43);

        // URL-safe, unpadded alphabet only.
        for s in [&pkce.verifier, &pkce.challenge] {
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "unexpected char in {s}"
            );
            assert!(!s.contains('='), "padding must be stripped: {s}");
            assert!(
                !s.contains('+') && !s.contains('/'),
                "must be url-safe: {s}"
            );
        }

        // The challenge is deterministically derived from the verifier.
        assert_eq!(pkce.challenge, Pkce::challenge_for(&pkce.verifier));
    }

    #[test]
    fn generate_is_random() {
        let a = Pkce::generate().unwrap();
        let b = Pkce::generate().unwrap();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }
}
