use crate::auth::types::AuthError;
use base64::prelude::{BASE64_URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Result<Pkce, AuthError> {
    let mut verifier_bytes = [0_u8; 32];
    getrandom::fill(&mut verifier_bytes)
        .map_err(|error| AuthError::new(format!("Could not generate PKCE verifier: {error}")))?;
    let verifier = BASE64_URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok(Pkce {
        verifier,
        challenge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins pi `src/auth/oauth/pkce.ts:22-33`.
    #[test]
    fn generates_32_random_bytes_and_their_sha256_base64url_challenge() {
        let generated = generate_pkce().expect("PKCE");
        assert_eq!(generated.verifier.len(), 43);
        assert!(!generated.verifier.contains(['+', '/', '=']));
        assert_eq!(
            generated.challenge,
            BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(generated.verifier.as_bytes()))
        );
    }
}
