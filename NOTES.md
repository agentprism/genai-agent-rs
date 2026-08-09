# Port notes — pi-ai → rust-genai-auth

Every constant, endpoint, grant type, and body field in the ChatGPT Codex flow
was taken verbatim from pi-ai. Source paths are relative to
`/home/vikash/genai-agent/pi/packages/ai/src/`. Line numbers are from the files
as read during the port.

## Codex constants — `auth/oauth/openai-codex.ts`

| Value                                              | pi file:line              | Rust location (`src/codex.rs`)          |
|----------------------------------------------------|---------------------------|-----------------------------------------|
| `CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann"`       | openai-codex.ts:26        | `CLIENT_ID`                             |
| `AUTH_BASE_URL = "https://auth.openai.com"`        | openai-codex.ts:27        | `AUTH_BASE_URL`                         |
| `AUTHORIZE_URL = ${base}/oauth/authorize`          | openai-codex.ts:28        | `AUTHORIZE_PATH` (`/oauth/authorize`)   |
| `TOKEN_URL = ${base}/oauth/token`                  | openai-codex.ts:29        | `TOKEN_PATH` (`/oauth/token`)           |
| `REDIRECT_URI = http://localhost:1455/auth/callback` | openai-codex.ts:30      | `REDIRECT_URI` (fixed loopback)         |
| `DEVICE_USER_CODE_URL = ${base}/api/accounts/deviceauth/usercode` | openai-codex.ts:31 | `DEVICE_USER_CODE_PATH`         |
| `DEVICE_TOKEN_URL = ${base}/api/accounts/deviceauth/token` | openai-codex.ts:32 | `DEVICE_TOKEN_PATH`                |
| `DEVICE_VERIFICATION_URI = ${base}/codex/device`   | openai-codex.ts:33        | `DEVICE_VERIFICATION_PATH`              |
| `DEVICE_REDIRECT_URI = ${base}/deviceauth/callback`| openai-codex.ts:34        | `DEVICE_REDIRECT_PATH`                  |
| `DEVICE_CODE_TIMEOUT_SECONDS = 15 * 60`            | openai-codex.ts:35        | `DEVICE_CODE_TIMEOUT_SECONDS`           |
| `OPENAI_CODEX_BROWSER_LOGIN_METHOD = "browser"`    | openai-codex.ts:36        | `BROWSER_LOGIN_METHOD`                  |
| `OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD = "device_code"` | openai-codex.ts:37   | `DEVICE_CODE_LOGIN_METHOD`              |
| `SCOPE = "openid profile email offline_access"`    | openai-codex.ts:38        | `SCOPE`                                 |
| `JWT_CLAIM_PATH = "https://api.openai.com/auth"`   | openai-codex.ts:39        | `jwt::JWT_CLAIM_PATH`                   |
| callback host env `PI_OAUTH_CALLBACK_HOST` / `127.0.0.1` | openai-codex.ts:44-46 | `loopback::CALLBACK_HOST_ENV`         |
| `originator` default `"pi"` (`createAuthorizationFlow`) | openai-codex.ts:294  | `DEFAULT_ORIGINATOR`                    |
| provider id `"openai-codex"`                       | providers/openai-codex.ts:9 | `PROVIDER_ID`                        |

### Authorize URL query params — `createAuthorizationFlow` (openai-codex.ts:299-311)

`response_type=code`, `client_id`, `redirect_uri`, `scope`, `code_challenge`,
`code_challenge_method=S256`, `state`, `id_token_add_organizations=true`,
`codex_cli_simplified_flow=true`, `originator` → reproduced in
`CodexAuth::begin_browser_login` in the same order.

### Token exchange — `exchangeAuthorizationCode` (openai-codex.ts:149-169)

`POST {base}/oauth/token`, `Content-Type: application/x-www-form-urlencoded`,
body: `grant_type=authorization_code`, `client_id`, `code`, `code_verifier`,
`redirect_uri` → `CodexAuth::exchange_authorization_code`.

### Token refresh — `refreshAccessToken` (openai-codex.ts:171-189)

`POST {base}/oauth/token`, `application/x-www-form-urlencoded`, body:
`grant_type=refresh_token`, `refresh_token`, `client_id` → `CodexAuth::refresh`.
(Cross-checked against the anthropic.ts refresh pattern, anthropic.ts:317-353.)

### Token response parsing — `readTokenResponse` (openai-codex.ts:126-147)

Requires `access_token`, `refresh_token`, and numeric `expires_in`; expiry =
`Date.now() + expires_in*1000` (openai-codex.ts:145) → `read_token_response`.

### Device usercode request — `startOpenAICodexDeviceAuth` (openai-codex.ts:191-233)

`POST {base}/api/accounts/deviceauth/usercode`, JSON `{ client_id }`; `404` ⇒
"device code login not enabled"; response requires `device_auth_id`, `user_code`,
and an `interval` (number **or** numeric string, openai-codex.ts:217) →
`CodexAuth::begin_device_login` + `parse_interval`.

### Device token poll — `pollOpenAICodexDeviceAuth` (openai-codex.ts:235-291)

`POST {base}/api/accounts/deviceauth/token`, JSON `{ device_auth_id, user_code }`;
`200` with `authorization_code` + `code_verifier` ⇒ complete; `403`/`404` ⇒
pending; body `error.code`/`error` of `deviceauth_authorization_pending` ⇒
pending; `slow_down` ⇒ slow down; else failed → `CodexAuth::poll_device_token` +
`error_code_of`. The device-flow exchange uses `DEVICE_REDIRECT_URI` and the
server-issued `code_verifier` (openai-codex.ts:437-442).

### chatgpt_account_id — `getAccountId` / `credentialsFromToken` (openai-codex.ts:396-416)

Decode the JWT payload, read
`payload["https://api.openai.com/auth"]["chatgpt_account_id"]`, require a
non-empty string → `jwt::extract_chatgpt_account_id`; the flow errors with
"Failed to extract accountId from token" when absent (openai-codex.ts:405-407),
reproduced by `Error::MissingAccountId` (gated by
`CodexConfig::require_account_id`, default `true`).

Deliberate correction: pi decodes the JWT segment with `atob` (standard base64,
openai-codex.ts:108). JWT payloads are actually base64url; this crate decodes
base64url (`URL_SAFE_NO_PAD`), which is the correct encoding and a strict
superset of what pi handled. No signature verification is performed (local claim
read only).

### `state` — `createState` (openai-codex.ts:66-71)

`randomBytes(16).toString("hex")` → `random_state()` (16 CSPRNG bytes, hex).

### `parseAuthorizationInput` (openai-codex.ts:73-101)

URL → query `code`/`state`; `#` → `code#state`; `code=` → query fragment; else
bare code → `parse_authorization_input`, with the state-mismatch check at
openai-codex.ts:485 reproduced in `complete_browser_login`.

## PKCE — `auth/oauth/pkce.ts`

| Behavior                                            | pi file:line   | Rust location (`src/pkce.rs`) |
|-----------------------------------------------------|----------------|-------------------------------|
| base64url = base64, `+`→`-`, `/`→`_`, strip `=`     | pkce.ts:9-15   | `URL_SAFE_NO_PAD` engine      |
| verifier = base64url(32 random bytes)               | pkce.ts:23-25  | `Pkce::generate`              |
| challenge = base64url(SHA-256(verifier))            | pkce.ts:28-31  | `Pkce::challenge_for`         |

Verified against the RFC 7636 Appendix B known-answer vector in a unit test.

## Device-code loop — `auth/oauth/device-code.ts`

| Constant / behavior                                          | pi file:line       | Rust (`src/device_code.rs`)        |
|--------------------------------------------------------------|--------------------|------------------------------------|
| `MINIMUM_INTERVAL_MS = 1000`                                 | device-code.ts:5   | `MINIMUM_INTERVAL_MS`              |
| `DEFAULT_POLL_INTERVAL_SECONDS = 5`                          | device-code.ts:7   | `DEFAULT_POLL_INTERVAL_SECONDS`    |
| `SLOW_DOWN_INTERVAL_INCREMENT_MS = 5000`                     | device-code.ts:9   | `SLOW_DOWN_INTERVAL_INCREMENT_MS`  |
| `CANCEL_MESSAGE = "Login cancelled"`                         | device-code.ts:1   | (cancellation = drop the future)  |
| `TIMEOUT_MESSAGE = "Device flow timed out"`                  | device-code.ts:2   | `Error::DeviceTimeout`             |
| slow-down timeout message                                    | device-code.ts:3-4 | `Error::DeviceSlowDownTimeout`     |
| deadline = `now + expires*1000`; unset ⇒ `+Infinity`         | device-code.ts:47-50 | `deadline: Option<Instant>`      |
| `interval = max(1000, floor(interval*1000))`                 | device-code.ts:51-54 | `clamp_interval_ms`              |
| `slow_down`: server interval else `+5000ms`                  | device-code.ts:76-87 | slow-down arm in loop            |
| sleep `min(interval, remaining)`                             | device-code.ts:89-94 | `min(interval_ms, rem)`          |

Cancellation: pi threads an `AbortSignal`; the idiomatic Rust equivalent is
dropping the returned future (each `sleep`/poll is a cancellation point), so no
explicit signal is exposed.

## Credential shape / store — `auth/types.ts`, `auth/credential-store.ts`

| pi concept                                          | pi file:line     | Rust                              |
|-----------------------------------------------------|------------------|-----------------------------------|
| `OAuthCredential { type:"oauth", access, refresh, expires }` | types.ts:24-34 | `credential::OAuthCredential` (serde-renamed to those keys) |
| `accountId` on the credential                       | openai-codex.ts:414 | `OAuthCredential.account_id` (`accountId`) |
| `[key: string]: unknown` extra keys                 | types.ts:28      | `OAuthCredential.extra` (`#[serde(flatten)]`) |
| `CredentialStore` read / modify / delete / list     | types.ts:65-94   | `store::CredentialStore` trait    |
| serialized `modify` read-modify-write               | types.ts:86-90   | `CredentialStore::modify` (locked in `FileCredentialStore`) |

`toAuth(credential) => { apiKey: credential.access }` (openai-codex.ts:541-543)
maps to `genai_integration::codex_auth_resolver` returning
`AuthData::from_single(access_token)`.

## Intentional deviations from pi

1. **JWT decode uses base64url** (correct for JWT) rather than pi's `atob`
   (standard base64). Superset behavior; see above.
2. **Expiry skew applied at check time, not store time.** The Codex flow stores
   the raw `expires` (openai-codex.ts:145); `OAuthCredential::is_expired(skew)`
   applies the margin when checking. (pi's anthropic.ts:230,351 bakes a 5-minute
   margin into the stored value; `DEFAULT_EXPIRY_SKEW` = 5 minutes reproduces
   that effect for Codex.)
3. **Credential path** is `~/.genai/auth.json` (env override `GENAI_AUTH_FILE`),
   a sane genai-side default, since pi's store is app-injected rather than a
   fixed path.
4. **`require_account_id` toggle** (default `true` = pi parity) lets non-Codex
   or test callers relax the "must contain `chatgpt_account_id`" requirement.
5. **Cancellation** via future-drop instead of `AbortSignal` (see device-code).
6. **Resolver-serialized refresh.** pi runs OAuth refresh inside its serialized
   per-provider `modify` lock (types.ts:54-57,86-90). The Rust
   `CredentialStore::modify` closure is synchronous, so the async refresh cannot
   run inside it; instead `genai_integration::CodexTokenResolver` serializes the
   load-check-refresh-store sequence with a `tokio::sync::Mutex` (in-process) and
   a best-effort `flock` on a `<auth.json>.lock` sidecar (cross-process, via the
   new `CredentialStore::lock_path`), with a double-checked expiry re-read inside
   the lock. This prevents two concurrent requests from both POSTing the same
   rotating refresh token (one would get `invalid_grant`). Cross-process locking
   is best-effort and only covers refreshes routed through the resolver.
7. **Redacted `Debug`.** Every type holding a secret (`OAuthCredential`, `Pkce`,
   `PendingBrowserLogin`, and the internal `DeviceTokenSuccess` / `OAuthToken` /
   `ParsedAuth`) has a hand-written `Debug` that prints the secret token /
   verifier / code fields as `"<redacted>"` while keeping non-secret metadata
   visible. pi has no equivalent (JS objects log verbatim).
8. **Overflow-safe expiry.** `expires = Date.now() + expires_in*1000`
   (openai-codex.ts:145) is computed with clamp + `saturating_add` so a hostile
   `expires_in` (e.g. `1e300`, `NaN`, `±∞`, negative) cannot panic (debug) or
   wrap (release): finite huge values saturate to a far-future expiry; non-finite
   or negative values collapse to "already expired" (fail-closed).
9. **Empty-string tokens rejected.** An empty `access_token`/`refresh_token` is
   treated as a missing field (`TokenResponseMissingFields`), matching pi's
   truthiness check (openai-codex.ts:138 `!json.access_token`).
10. **`parseAuthorizationInput` / empty-interval — stricter but fail-closed.** A
    parsed empty `code` is treated as no code (`.filter(|c| !c.is_empty())` in
    `complete_browser_login`), and a missing/unparseable device `interval` is
    rejected as `InvalidDeviceCodeResponse` rather than silently defaulting.
    Both are intentionally stricter than pi and fail closed.
11. **Temp-file hardening.** The atomic-write temp file is created with
    `O_CREAT | O_EXCL | O_NOFOLLOW` at mode `0600` and a CSPRNG-random suffix
    (was pid+timestamp); an existing target directory is also tightened to
    `0700` (best-effort). pi's store is app-injected, so this is a genai-side
    default with no direct pi analogue.

## Error redaction (caller responsibility)

Error variants carrying a response `body`/message (`TokenRequest`,
`TokenResponseMissingFields`, `DeviceCodeRequestFailed`,
`InvalidDeviceCodeResponse`, `DeviceAuth`) may include response data (and, in the
`missing fields` case, token material). Their rustdoc warns callers to redact the
body before logging; the crate itself never logs.
