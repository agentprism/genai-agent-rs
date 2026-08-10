//! Minimal, unverified JWT payload decoding — only used to read the
//! `chatgpt_account_id` claim out of the Codex access token.
//!
//! Faithful to pi-ai's `decodeJwt` / `getAccountId` (openai-codex.ts:103-113, 396-401),
//! with one deliberate correction: pi calls `atob` (standard base64) on the JWT
//! payload segment, which is actually base64url; here we decode base64url
//! (`URL_SAFE_NO_PAD`), which is the correct JWT encoding and a strict superset of
//! what pi handled. No signature verification is performed — this is a local claim
//! read, not a trust decision.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;

/// The Codex JWT claim namespace that wraps `chatgpt_account_id`.
///
/// Ported verbatim from `JWT_CLAIM_PATH` (openai-codex.ts:39).
pub const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

/// Decode (without verifying) the payload segment of a compact JWS/JWT.
///
/// Returns `None` if the token does not have exactly three dot-separated
/// segments, the payload is not valid base64url, or it is not valid JSON.
pub fn decode_jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Extract the ChatGPT account id from a Codex access token.
///
/// Reads `payload["https://api.openai.com/auth"]["chatgpt_account_id"]` and
/// returns it only when it is a non-empty string (openai-codex.ts:396-401).
pub fn extract_chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    let account_id = payload
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()?;
    if account_id.is_empty() {
        None
    } else {
        Some(account_id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(payload: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        let sig = URL_SAFE_NO_PAD.encode(b"signature-not-verified");
        format!("{header}.{body}.{sig}")
    }

    #[test]
    fn extracts_account_id_from_nested_claim() {
        let token = make_jwt(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_abc123" },
            "sub": "user_1",
        }));
        assert_eq!(
            extract_chatgpt_account_id(&token).as_deref(),
            Some("acct_abc123")
        );
    }

    #[test]
    fn returns_none_when_claim_missing() {
        let token = make_jwt(&serde_json::json!({ "sub": "user_1" }));
        assert_eq!(extract_chatgpt_account_id(&token), None);
    }

    #[test]
    fn returns_none_for_empty_account_id() {
        let token = make_jwt(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "" },
        }));
        assert_eq!(extract_chatgpt_account_id(&token), None);
    }

    #[test]
    fn returns_none_for_non_jwt() {
        assert_eq!(extract_chatgpt_account_id("not-a-jwt"), None);
        assert_eq!(extract_chatgpt_account_id("a.b"), None);
    }
}
