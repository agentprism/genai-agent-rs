//! The on-disk OAuth credential shape and its expiry bookkeeping.
//!
//! The struct field names are Rust-idiomatic, but serde renames map them to
//! pi-ai's stored JSON keys so the file format stays pi-parity:
//!
//! | Rust field         | JSON key    | pi source (types.ts / openai-codex.ts) |
//! |--------------------|-------------|----------------------------------------|
//! | `kind`             | `type`      | types.ts:33 (`type: "oauth"`)          |
//! | `access_token`     | `access`    | types.ts:27, openai-codex.ts:411       |
//! | `refresh_token`    | `refresh`   | types.ts:26, openai-codex.ts:412       |
//! | `expires_at_ms`    | `expires`   | types.ts:28, openai-codex.ts:413       |
//! | `account_id`       | `accountId` | openai-codex.ts:414                     |
//! | `scope`            | `scope`     | (extra; not populated by the Codex flow)|
//!
//! pi's `OAuthCredentials` also carries `[key: string]: unknown` (types.ts:28),
//! so any additional keys survive a round-trip via `extra`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The canonical credential `type` tag (pi stores `type: "oauth"`, types.ts:33).
pub const OAUTH_CREDENTIAL_TYPE: &str = "oauth";

/// Default clock-skew margin applied by [`OAuthCredential::is_expired`].
///
/// Anthropic's pi flow bakes a 5-minute margin into the stored expiry
/// (anthropic.ts:230,351); the Codex flow stores the raw expiry
/// (openai-codex.ts:145), so we apply the same 5-minute margin at *check* time.
pub const DEFAULT_EXPIRY_SKEW: Duration = Duration::from_secs(5 * 60);

/// A stored OAuth credential (pi's `OAuthCredential`, types.ts:31-34).
///
/// `Debug` is hand-written and **redacts the secret token fields**
/// (`access_token`, `refresh_token`): both print as `"<redacted>"` (the
/// refresh token preserving only its `Some`/`None` presence), while non-secret
/// metadata (`kind`, `expires_at_ms`, `account_id`, `scope`, `extra`) is shown.
/// This keeps `{:?}` / structured logs from leaking bearer secrets.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    /// Type tag; always `"oauth"` for this crate.
    #[serde(rename = "type", default = "default_oauth_type")]
    pub kind: String,

    /// The bearer access token.
    #[serde(rename = "access")]
    pub access_token: String,

    /// The refresh token, when the grant returned one (`offline_access`).
    #[serde(rename = "refresh", default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,

    /// Absolute expiry as Unix epoch milliseconds (`Date.now() + expires_in*1000`).
    #[serde(rename = "expires", default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,

    /// ChatGPT account id extracted from the access-token JWT.
    #[serde(rename = "accountId", default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,

    /// Granted scope, when the provider echoes it back. Not populated by the
    /// Codex flow (pi parity), but part of the canonical shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,

    /// Any additional keys present on disk (`[key: string]: unknown`, types.ts:28).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_oauth_type() -> String {
    OAUTH_CREDENTIAL_TYPE.to_string()
}

impl std::fmt::Debug for OAuthCredential {
    /// Redacts the secret token fields; see the type-level docs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCredential")
            .field("kind", &self.kind)
            .field("access_token", &"<redacted>")
            // Preserve presence (Some/None) without revealing the value.
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("account_id", &self.account_id)
            .field("scope", &self.scope)
            .field("extra", &self.extra)
            .finish()
    }
}

impl OAuthCredential {
    /// Build a canonical `oauth` credential.
    pub fn new(
        access_token: impl Into<String>,
        refresh_token: Option<String>,
        expires_at_ms: Option<i64>,
        account_id: Option<String>,
    ) -> Self {
        Self {
            kind: OAUTH_CREDENTIAL_TYPE.to_string(),
            access_token: access_token.into(),
            refresh_token,
            expires_at_ms,
            account_id,
            scope: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Whether this credential is (or is about to be) expired, given a skew margin.
    ///
    /// Returns `true` when `now + skew >= expires_at_ms`. A credential with no
    /// recorded expiry is treated as expired (unknown freshness -> refresh).
    pub fn is_expired(&self, skew: Duration) -> bool {
        self.is_expired_at(now_unix_ms(), skew)
    }

    /// [`is_expired`](Self::is_expired) against an explicit `now` (testable).
    pub fn is_expired_at(&self, now_ms: i64, skew: Duration) -> bool {
        match self.expires_at_ms {
            Some(exp) => now_ms.saturating_add(skew.as_millis() as i64) >= exp,
            None => true,
        }
    }

    /// Remaining lifetime relative to `now`, or `None` if unknown / already expired.
    pub fn expires_in_at(&self, now_ms: i64) -> Option<Duration> {
        let exp = self.expires_at_ms?;
        let remaining = exp - now_ms;
        if remaining <= 0 {
            None
        } else {
            Some(Duration::from_millis(remaining as u64))
        }
    }
}

/// Current wall-clock time as Unix epoch milliseconds (matches JS `Date.now()`).
pub fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_pi_on_disk_shape() {
        // The exact JSON pi writes for a Codex credential.
        let json = r#"{"type":"oauth","access":"acc-tok","refresh":"ref-tok","expires":1750000000000,"accountId":"acct_123"}"#;
        let cred: OAuthCredential = serde_json::from_str(json).unwrap();
        assert_eq!(cred.kind, "oauth");
        assert_eq!(cred.access_token, "acc-tok");
        assert_eq!(cred.refresh_token.as_deref(), Some("ref-tok"));
        assert_eq!(cred.expires_at_ms, Some(1_750_000_000_000));
        assert_eq!(cred.account_id.as_deref(), Some("acct_123"));

        // Re-serialize and confirm the pi key names survive (order-independent).
        let value: serde_json::Value = serde_json::to_value(&cred).unwrap();
        assert_eq!(value["type"], "oauth");
        assert_eq!(value["access"], "acc-tok");
        assert_eq!(value["refresh"], "ref-tok");
        assert_eq!(value["expires"], 1_750_000_000_000_i64);
        assert_eq!(value["accountId"], "acct_123");
    }

    #[test]
    fn preserves_unknown_keys_and_scope() {
        let json = r#"{"type":"oauth","access":"a","scope":"openid email","org":"org_9"}"#;
        let cred: OAuthCredential = serde_json::from_str(json).unwrap();
        assert_eq!(cred.scope.as_deref(), Some("openid email"));
        assert_eq!(
            cred.extra.get("org").and_then(|v| v.as_str()),
            Some("org_9")
        );

        let back = serde_json::to_value(&cred).unwrap();
        assert_eq!(back["org"], "org_9");
        assert_eq!(back["scope"], "openid email");
        // Absent optionals must not be serialized.
        assert!(back.get("refresh").is_none());
        assert!(back.get("expires").is_none());
        assert!(back.get("accountId").is_none());
    }

    #[test]
    fn programmatic_round_trip() {
        let cred = OAuthCredential::new("a", Some("r".into()), Some(42), Some("acct".into()));
        let s = serde_json::to_string(&cred).unwrap();
        let back: OAuthCredential = serde_json::from_str(&s).unwrap();
        assert_eq!(cred, back);
    }

    #[test]
    fn debug_redacts_secret_tokens() {
        let cred = OAuthCredential::new(
            "super-secret-access-token",
            Some("super-secret-refresh-token".into()),
            Some(1_750_000_000_000),
            Some("acct_visible".into()),
        );
        let dbg = format!("{cred:?}");
        // Secrets must not appear.
        assert!(
            !dbg.contains("super-secret-access-token"),
            "access token leaked in Debug: {dbg}"
        );
        assert!(
            !dbg.contains("super-secret-refresh-token"),
            "refresh token leaked in Debug: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "expected redaction marker: {dbg}"
        );
        // Non-secret metadata stays visible.
        assert!(
            dbg.contains("acct_visible"),
            "account id should be visible: {dbg}"
        );
        assert!(
            dbg.contains("1750000000000"),
            "expiry should be visible: {dbg}"
        );
        // Presence of the refresh token is still observable (Some vs None).
        assert!(
            dbg.contains("refresh_token: Some"),
            "refresh presence lost: {dbg}"
        );

        // A credential without a refresh token shows None (not a redaction).
        let no_refresh = OAuthCredential::new("a", None, None, None);
        let dbg2 = format!("{no_refresh:?}");
        assert!(
            dbg2.contains("refresh_token: None"),
            "expected None: {dbg2}"
        );
    }

    #[test]
    fn is_expired_boundaries() {
        let cred = OAuthCredential::new("a", None, Some(1000), None);
        let skew = Duration::from_millis(100);

        // now + skew < exp -> fresh
        assert!(!cred.is_expired_at(899, skew));
        // now + skew == exp -> expired (>=)
        assert!(cred.is_expired_at(900, skew));
        // now + skew > exp -> expired
        assert!(cred.is_expired_at(901, skew));

        // Zero skew boundary: now == exp is expired.
        assert!(!cred.is_expired_at(999, Duration::ZERO));
        assert!(cred.is_expired_at(1000, Duration::ZERO));
    }

    #[test]
    fn unknown_expiry_is_treated_as_expired() {
        let cred = OAuthCredential::new("a", Some("r".into()), None, None);
        assert!(cred.is_expired_at(0, Duration::ZERO));
        assert_eq!(cred.expires_in_at(0), None);
    }

    #[test]
    fn expires_in_computes_remaining() {
        let cred = OAuthCredential::new("a", None, Some(5000), None);
        assert_eq!(cred.expires_in_at(2000), Some(Duration::from_millis(3000)));
        assert_eq!(cred.expires_in_at(5000), None);
        assert_eq!(cred.expires_in_at(6000), None);
    }
}
