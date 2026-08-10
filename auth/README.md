# rust-genai-auth

OAuth login, on-disk token cache, and refresh for [`genai`](https://crates.io/crates/genai).

This crate is the Rust structural equivalent of pi-ai's `packages/ai/src/auth/`
module. It owns the OAuth flows, the credential store, and token refresh so that
`genai` and higher-level agents (e.g. `rust-genai-agent`) stay **auth-agnostic** —
they only ever see a bearer token via a `genai` `AuthResolver`.

This first release delivers the **ChatGPT Codex** (OpenAI Codex /
ChatGPT-subscription) OAuth flow, ported faithfully from pi-ai's
`auth/oauth/openai-codex.ts`, `pkce.ts`, and `device-code.ts`. Every endpoint,
client id, scope, grant type, and body field is cited to its pi source line in
the code and in [`NOTES.md`](./NOTES.md).

## What it provides

- `OAuthCredential` — the stored credential shape. Rust-idiomatic field names,
  serde-mapped to pi's on-disk `auth.json` keys (`type`/`access`/`refresh`/
  `expires`/`accountId`), with expiry math (`is_expired(skew)`).
- `CredentialStore` — the app-owned storage seam, plus a default file-backed
  `FileCredentialStore` (atomic writes, `0600`, env-overridable path).
- `Pkce` — RFC 7636 S256 PKCE generation.
- `CodexAuth` — the Codex OAuth flow:
  - `begin_browser_login()` / `complete_browser_login()` (PKCE browser flow),
  - `begin_device_login()` / `poll_device_login()` (headless device-code flow),
  - `refresh()` (`grant_type=refresh_token`),
  - `exchange_authorization_code()` (low-level code exchange).
- Optional loopback redirect-capture server (feature `loopback`).
- Optional `genai` `AuthResolver` adapter (feature `genai`).

## The ChatGPT Codex flow

Two login methods, both PKCE-based, mirroring pi's `openaiCodexOAuth`:

### Browser login (application owns the browser + redirect capture)

The core is headless and never opens a browser or binds a socket. You call
`begin_browser_login()` to get the authorize URL and a pending PKCE
verifier/state; **your application** opens the URL and captures the loopback
redirect (or asks the user to paste the redirect URL / code); then you call
`complete_browser_login()` to exchange the code for a credential.

```rust
use rust_genai_auth::{CodexAuth, FileCredentialStore, CredentialStore, codex};

# async fn demo() -> rust_genai_auth::Result<()> {
let auth = CodexAuth::new();
let store = FileCredentialStore::with_default_path()?;

let pending = auth.begin_browser_login()?;
println!("Open in your browser:\n{}", pending.authorize_url);

// Application opens the browser and obtains the redirect input:
let redirect_input = "http://localhost:1455/auth/callback?code=CODE&state=...";

let credential = auth.complete_browser_login(&pending, redirect_input).await?;
store.store(codex::PROVIDER_ID, &credential)?;
# Ok(()) }
```

The loopback redirect capture (pi's `http.createServer` on
`http://localhost:1455/auth/callback`) is provided as an **optional** helper
behind the `loopback` feature so the core stays fully headless-testable:

```rust,ignore
use rust_genai_auth::loopback::{capture_redirect, LoopbackConfig};

let pending = auth.begin_browser_login()?;
// open_in_browser(&pending.authorize_url);
let code = capture_redirect(&LoopbackConfig::default(), &pending.state).await?;
let credential = auth.complete_browser_login(&pending, &code).await?;
```

### Device-code login (headless)

```rust
use rust_genai_auth::{CodexAuth, FileCredentialStore, CredentialStore, codex};

# async fn demo() -> rust_genai_auth::Result<()> {
let auth = CodexAuth::new();
let store = FileCredentialStore::with_default_path()?;

let begin = auth.begin_device_login().await?;
println!("Go to {} and enter code {}", begin.verification_uri, begin.user_code);

let credential = auth.poll_device_login(&begin).await?; // polls until authorized
store.store(codex::PROVIDER_ID, &credential)?;
# Ok(()) }
```

## Credential path

The default file-backed store writes a pi-parity `auth.json` — a JSON object
keyed by provider id, one credential per provider — to:

1. `$GENAI_AUTH_FILE` if set (full path; a leading `~/` is expanded), else
2. `~/.genai/auth.json`.

The Codex credential is stored under the provider id `openai-codex`
(`rust_genai_auth::OPENAI_CODEX_PROVIDER_ID`). Unrelated provider entries in the
same file are preserved across writes.

## genai `AuthResolver` wiring (feature = `genai`)

Enable the `genai` feature and install the resolver on a `genai::Client`. On each
request the resolver loads the stored credential, refreshes it if it is expired
(persisting the fresh token through the store), and hands `genai` the bearer:

```rust,ignore
use std::sync::Arc;
use rust_genai_auth::{CodexAuth, FileCredentialStore, OPENAI_CODEX_PROVIDER_ID};
use rust_genai_auth::genai_integration::codex_auth_resolver;
use genai::Client;

let auth = Arc::new(CodexAuth::new());
let store = Arc::new(FileCredentialStore::with_default_path()?);
let resolver = codex_auth_resolver(auth, store, OPENAI_CODEX_PROVIDER_ID);

let client = Client::builder().with_auth_resolver(resolver).build();
```

The resolved value is the bearer access token, mirroring pi's
`toAuth(credential) => { apiKey: credential.access }`. The ChatGPT backend also
needs a `chatgpt-account-id` header and the `chatgpt.com/backend-api/codex` base
URL; that provider wiring is the application's responsibility (the account id is
available on `OAuthCredential::account_id`), exactly as in pi.

**Install it provider-scoped.** The resolver returns the Codex bearer for every
request regardless of the `ModelIden`, so install it on a client that only
serves the Codex/ChatGPT provider — not as a global resolver on a multi-provider
client.

**Serialized refresh (no double-refresh race).** Concurrent requests cannot
double-refresh a rotating refresh token: the resolver serializes the
load-check-refresh-store sequence behind a `tokio::sync::Mutex` (in-process) plus
a best-effort `flock` advisory lock on a `<auth.json>.lock` sidecar
(cross-process), re-checking expiry inside the lock so late waiters reuse the
freshly stored token instead of issuing a second refresh. This mirrors pi
running refresh inside its serialized `modify` lock. The coordination object,
`genai_integration::CodexTokenResolver`, is public for apps with custom wiring.
Cross-process serialization is best-effort and covers refreshes routed through
the resolver; credential writes made outside it are not `flock`-serialized.

### Fork-branch caveat

The `genai` feature pulls `genai` as a **path dependency** to `../rust-genai`,
which is currently parked on the `feat/exec-interceptors-error-headers-tool-parts`
branch. The **default build depends on nothing from genai** — only
`--features genai` compiles the adapter. If/when the fork lands upstream, switch
the dependency in `Cargo.toml` to a versioned crates.io release.

## Features

| Feature    | Default | Effect                                                                            |
|------------|---------|-----------------------------------------------------------------------------------|
| `loopback` | off     | Loopback redirect-capture server for the browser flow (tokio sockets).            |
| `genai`    | off     | `genai` `AuthResolver` adapter (serialized refresh); adds `../rust-genai`, `fs2`, `tokio/sync`. |

## Dependencies (kept lean)

`reqwest` (async HTTP, rustls TLS), `serde`/`serde_json` (JSON), `tokio` (`time`,
for the device-code poll sleeps), `sha2` + `base64` (PKCE S256 / base64url),
`getrandom` (CSPRNG for the verifier, CSRF state, and temp-file suffix), and
`thiserror` (error enum, compile-time only). On unix, `libc` (already in the
tree) supplies the `O_NOFOLLOW` open flag for the credential store's temp file.

The optional `genai` feature additionally pulls `tokio/sync` (the resolver's
serialization mutex) and `fs2` — a tiny, `libc`-only, safe wrapper around
`flock(2)` — for the resolver's cross-process advisory lock. The default build
pulls neither.

## Security

- **Tokens at rest.** Access and refresh tokens are written to the credential
  file in plaintext JSON, like pi's `auth.json`. Protect this file the same way
  you protect an API key.
- **`0600` / `0700`.** On unix the credential file is created `0600`
  (owner read/write only) and the parent directory `0700` — an **existing**
  target directory is tightened to `0700` too (best-effort). Writes are atomic
  (temp file in the same directory + `rename`) so a crash cannot leave a
  half-written or world-readable file. The temp file is created with
  `O_CREAT | O_EXCL | O_NOFOLLOW` at mode `0600` with a CSPRNG-random suffix, so
  it never overwrites, follows a symlink, or uses a predictable name.
- **Env override.** `GENAI_AUTH_FILE` overrides the path — point it at a
  `tmpfs`, an age-encrypted volume, or a per-session directory if you do not want
  tokens on your home partition. Prefer a dedicated directory: its parent is
  tightened to `0700`.
- **Redacted `Debug`.** Secret-bearing types (`OAuthCredential`, `Pkce`,
  `PendingBrowserLogin`, and the internal token/verifier/code carriers) have
  hand-written `Debug` impls that print secret fields as `"<redacted>"`, so
  `{:?}` / structured logs cannot leak the access token, refresh token, PKCE
  verifier, or authorization code. Error variants that carry a response `body`
  are documented as possibly-sensitive — redact them before logging.
- **PKCE + CSRF state.** The browser flow uses S256 PKCE and a random `state`
  that must match on the redirect. The loopback host honors
  `PI_OAUTH_CALLBACK_HOST` (default `127.0.0.1`) — keep it on loopback.

## Testing

Everything is offline — no network to `chatgpt.com` / `auth.openai.com` and no
secrets. HTTP flows are exercised against a local mock server (a tokio
`TcpListener` returning canned JSON) that captures each request so the tests can
assert the exact URL, method, headers, and body fields.

```bash
cargo test                 # core (default features)
cargo test --all-features  # + genai adapter + loopback
```

## License

MIT OR Apache-2.0.
