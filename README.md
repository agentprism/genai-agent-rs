# rust-genai-codex

`CodexStreamFn` — a [`rust-genai-agent`](../rust-genai-agent) `StreamFn` that talks
to the **ChatGPT-subscription Codex backend** (`chatgpt.com/backend-api`),
consuming OAuth tokens from [`rust-genai-auth`](../rust-genai-auth).

It is the Rust equivalent of pi-ai's
`packages/ai/src/api/openai-codex-responses.ts` (the `stream` export): the OpenAI
**Responses** API spoken over the ChatGPT Codex backend, with the same
WebSocket-with-SSE-fallback transport model.

## Why a separate crate

- `rust-genai-agent` stays **auth-agnostic** (it only knows the `StreamFn` trait
  and the assistant event protocol).
- `rust-genai-auth` stays **transport-agnostic** (it only knows OAuth, the token
  cache, and refresh).

`CodexStreamFn` needs **both** — a `StreamFn` that consumes OAuth tokens — so it
lives here, keeping the other two crates decoupled. It depends on all three
sibling crates (`rust-genai-agent`, `rust-genai-auth`, `genai`) by path only.

## What it does, per request

1. Resolves a **fresh bearer + `chatgpt-account-id`** from a `TokenSource`
   (production: the auth crate's `CodexTokenResolver`, which handles
   expiry → refresh → persist with the double-refresh race fix). The account id
   is decoded from the bearer's JWT claim (`https://api.openai.com/auth`
   → `chatgpt_account_id`), exactly as pi does.
2. Builds the OpenAI **Responses** request body from the agent's `LlmContext`
   + options (`model`, `store:false`, `stream:true`, `instructions`, `input`,
   `text.verbosity`, `include:["reasoning.encrypted_content"]`, `tool_choice`,
   `parallel_tool_calls`, and optional `temperature` / `service_tier` / `tools` /
   `reasoning`).
3. Streams the response over **WebSocket (with SSE fallback)** or **SSE only**.
4. Maps the Codex Responses event stream onto the crate's assistant event
   protocol (start / text deltas / thinking / tool calls / done / error) via the
   **same `AssistantAccumulator`** that `GenaiStreamFn` uses, so the
   `AssistantMessageEventStream` contract is byte-for-byte the one every other
   stream function in the crate produces.

Every failure — token resolution, request send, non-2xx handshake, transport or
protocol error, and cancellation — is reported **in-band** as a terminal error
event. Nothing is thrown, and no stream ends without a terminal event.
Cancellation yields a `StopReason::Aborted` terminal.

## Wiring (auth crate → CodexStreamFn → Agent)

```rust
use std::sync::Arc;
use rust_genai_codex::{CodexStreamFn, StaticTokenSource, Transport};

// 1. A token source. Tests / short-lived tools can use a fixed token:
let token_source = Arc::new(StaticTokenSource::new("bearer-jwt", "acct_123"));

// 2. The stream function (defaults: base URL chatgpt.com/backend-api,
//    transport Auto = WebSocket with SSE fallback, originator "pi",
//    User-Agent "pi (<platform> <release>; <arch>)" — pi's shape, overridable
//    via .with_user_agent(..)).
let stream_fn = Arc::new(
    CodexStreamFn::new(token_source)
        .with_transport(Transport::Auto),
);

// 3. Install `stream_fn` on an agent as its StreamFn (see rust-genai-agent's
//    AgentConfig / set_default_stream_fn).
```

### Production token source (feature `auth-resolver`)

Enable the `auth-resolver` feature to build a refreshing token source from the
auth crate's `CodexTokenResolver` (fresh bearer + refresh + persist; account id
derived from the JWT):

```rust,ignore
use std::sync::Arc;
use rust_genai_auth::{CodexAuth, FileCredentialStore, OPENAI_CODEX_PROVIDER_ID};
use rust_genai_auth::genai_integration::CodexTokenResolver;
use rust_genai_codex::{CodexStreamFn, ResolverTokenSource};

let auth = Arc::new(CodexAuth::new());
let store = Arc::new(FileCredentialStore::with_default_path()?);
let resolver = Arc::new(CodexTokenResolver::new(auth, store, OPENAI_CODEX_PROVIDER_ID));

let token_source = Arc::new(ResolverTokenSource::new(resolver));
let stream_fn = CodexStreamFn::new(token_source);
```

Or pass any `Fn() -> impl Future<Output = Result<CodexToken>>` closure — the
`dyn Fn -> Future<(bearer, account_id)>` shape — as the token source.

## Model selection

The model id comes from the agent's `AgentState.model` (sent verbatim as the
Responses `model` field); `CodexStreamFn` does not pick a model.

**A ChatGPT-subscription Codex account does not accept the `-codex`-suffixed
model slugs.** Verified live (2026-08-09) against a real ChatGPT account: the
backend rejects `gpt-5-codex`, `gpt-5.1-codex`, `gpt-5.2-codex`, and
`gpt-5.3-codex` with
`{"detail":"The '<model>' model is not supported when using Codex with a ChatGPT
account."}`. A general model such as **`gpt-5.6-sol`** is accepted and streams
normally. Use a general (non-`-codex`) slug your subscription exposes; the exact
set is account/plan-dependent, so treat a `not supported` detail as "wrong slug,"
not a transport failure.

## Transport modes

Configured via `CodexStreamFn::with_transport(..)` or the per-request
`StreamRequest.transport` advisory (the request wins when it is non-`Auto`;
otherwise the instance default applies):

| Mode               | Behavior                                                                 |
|--------------------|--------------------------------------------------------------------------|
| `Sse`              | SSE only (`reqwest`).                                                     |
| `Websocket`        | WebSocket (`tokio-tungstenite`), **falling back to SSE** on a pre-commit transport failure. |
| `WebsocketCached`  | Same as `Websocket` here — the cross-request WS connection cache is not ported (see below). |
| `Auto` *(default)* | Mirrors pi's default: WebSocket with SSE fallback.                        |

**Fallback rule** (faithful to pi): the WebSocket path falls back to SSE only
when it fails at the **transport** level *before* the first assistant event is
committed (handshake/upgrade failure, connect timeout, an unexpected close, an
I/O error before the first frame, or an in-band
`{"type":"error","code":"websocket_connection_limit_reached"}` frame — pi treats
the connection-limit error as a pre-commit transport failure). Every **other**
application error delivered as a Codex `error` / `response.failed` frame is
terminal and does **not** fall back. A transport failure *after* the first frame
is committed becomes a terminal in-band error (no fallback), including a
connection-limit frame that arrives after commit.

### Documented simplifications vs pi

- The cross-request **WebSocket connection cache / `previous_response_id`
  continuation** (pi's `websocket-cached`) is not ported; each request opens a
  fresh single-shot WebSocket, so `WebsocketCached` behaves like `Websocket`.
- The pre-start **connection-limit / previous-response-not-found retry loop** is
  not ported as a *retry*; instead a pre-commit in-band connection-limit `error`
  frame falls back to SSE once (other in-band errors stay terminal).
- Request-body **zstd compression** (`Content-Encoding: zstd`) is not sent; the
  body is plain JSON (pi itself falls back to plain JSON, which the backend
  accepts).
- **Grammar / custom tools**, **deferred tool search**, and assistant
  **reasoning-item (encrypted_content) replay** are not ported.
- **Usage convention:** the final `AgentUsage.input_tokens` keeps OpenAI's
  inclusive `input_tokens` (cache reads reported separately in
  `cache_read_tokens`), matching `AgentUsage::from(genai::Usage)` and therefore
  `GenaiStreamFn`. pi instead subtracts cache tokens from its own `Usage.input`.

See [`NOTES.md`](./NOTES.md) for the full pi `file:line` mapping.

## Testing scope — no live e2e in CI

The test suite validates **protocol framing, headers, request-body fields, event
→ assistant-event mapping, WebSocket→SSE fallback, non-2xx handshake handling,
and cancellation** against **local mock servers only** (tokio `TcpListener`;
`tokio-tungstenite`'s server side for WebSocket; a raw HTTP responder streaming
`data:` lines for SSE). Tests use a **stub token source** — no real OAuth.

**End-to-end validation against the real ChatGPT backend is intentionally OUT OF
SCOPE for CI.** It requires a real ChatGPT subscription, live OAuth, and network
access, none of which belong in an offline test suite. The mock tests therefore
validate the wire protocol / headers / event mapping / fallback / cancellation
contract — not a real conversation with `chatgpt.com`.

## Security

- **Tokens come only from `rust-genai-auth`** (or a caller-supplied
  `TokenSource`). This crate never performs OAuth, never reads the credential
  store directly, and never persists tokens. The refreshing `CodexTokenResolver`
  owns expiry/refresh/persist with in-process + cross-process serialization.
- The bearer is sent as `Authorization: Bearer …`; the **`chatgpt-account-id`**
  header is set from the account id derived from the bearer's JWT claim.
- Secrets are redacted in `Debug`: `CodexToken`'s bearer prints as `<redacted>`,
  matching the auth crate's `OAuthCredential`.
- Scope the stream function to the ChatGPT/Codex provider only — the token is a
  ChatGPT bearer and must not be sent to other providers.

## License

MIT OR Apache-2.0.
