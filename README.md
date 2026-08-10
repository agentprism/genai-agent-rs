# genai-agent-rs

A Rust port of [pi](https://github.com/earendil-works)'s agent stack, structured the
same way pi is: **two libraries**, one for the AI/transport layer and one for the
provider‑neutral agent loop.

| pi package | this workspace | crate |
|---|---|---|
| `@earendil-works/pi-ai` | `genai/` | **`genai-agentprism`** |
| `@earendil-works/pi-agent-core` | `agent/` | **`rust-genai-agent`** |

Both are published to crates.io; everything else pi keeps private (its CLI) stays
out of the published surface.

## The two crates

### `genai-agentprism` (= pi-ai)

An **owned fork of [`genai`](https://github.com/jeremychone/rust-genai)** (jeremychone's
multi‑provider client), kept in sync with upstream via `git subtree`, plus the pi‑ai
layer folded in:

- the **assistant / event / stream contract** — `AssistantMessage`,
  `AssistantMessageEvent`, `AssistantMessageEventStream`, `StopReason`,
  `AssistantAccumulator`, `StreamFn`, `StreamRequest`, `LlmContext`;
- **`GenaiStreamFn`** — the default backend that drives any genai provider, with a
  pi‑parity retry layer and request‑level exec hooks;
- **`genai::auth`** *(feature `auth`)* — OAuth login / token cache / refresh (mirrors
  pi‑ai's `auth/`); the first flow is ChatGPT Codex;
- **`genai::codex`** *(feature `codex`)* — the ChatGPT‑subscription **Codex** backend
  `StreamFn` (mirrors pi‑ai's `api/openai-codex-responses.ts`), WebSocket‑with‑SSE‑fallback.

The crate's library target is named `genai`, so code (and dependents, via
`package = "genai-agentprism"`) keeps writing `use genai::…`. With no features it is a
drop‑in `genai` provider client; the pi‑ai extras are additive.

**Features:** `auth`, `loopback` (auth's local redirect‑capture server), `codex`,
`codex-auth-resolver` (bridges `codex`'s token source to `auth`'s refreshing resolver),
plus the upstream genai features (`rustls-tls` *(default)*, `native-tls`,
`bedrock-sigv4`, `otel`). `rustls-tls` and `native-tls` are mutually exclusive.

### `rust-genai-agent` (= pi-agent-core)

The **provider‑neutral agent** — the streaming loop, tools, hooks, queues, `proxy`,
testing doubles, and `set_default_stream_fn`. It depends on `genai-agentprism` and
**re‑exports the stream contract**, so consumers write `rust_genai_agent::StreamFn`
exactly like pi‑agent-core. A concrete `StreamFn` (e.g. `GenaiStreamFn` or a Codex
backend) is injected; the loop itself is transport‑agnostic.

## Workspace layout

```
genai-agent-rs/
├── Cargo.toml          # [workspace] members = ["genai", "agent"]
├── genai/              # genai-agentprism  (pi-ai: fork + auth + codex)
└── agent/              # rust-genai-agent  (pi-agent-core)
```

## Build & test

```bash
cargo build --workspace
cargo test  --workspace                                   # default features
cargo test  -p genai-agentprism --features "auth loopback codex codex-auth-resolver"
cargo test  -p rust-genai-agent --all-features
```

> Some `genai/tests/tests_p_*.rs` are **live‑provider** tests and fail offline (no API
> keys / network) — that is upstream genai's normal offline behavior, not a defect.

## Keeping the fork in sync with upstream

We own the fork; upstream improvements are pulled in (pull‑only, never pushed):

```bash
git remote add upstream https://github.com/jeremychone/rust-genai   # once
git subtree pull --prefix=genai upstream main                       # merge upstream
```

Our additions live in new modules (`genai/src/{assistant,stream_fn,auth,codex,…}`), so
upstream merges mostly touch only `genai/src/lib.rs` and `genai/Cargo.toml`.

## Publishing

The two crates publish in order (cargo enforces it — `rust-genai-agent` pins an exact
`genai-agentprism` version):

```bash
cargo publish -p genai-agentprism      # pi-ai layer, first
cargo publish -p rust-genai-agent      # depends on it, second
```

## License

MIT OR Apache‑2.0. `genai-agentprism` is a fork of
[`jeremychone/rust-genai`](https://github.com/jeremychone/rust-genai) (MIT OR
Apache‑2.0); the agent layer is a port of
[`@earendil-works/pi-agent-core`](https://github.com/earendil-works).
