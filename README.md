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

The subtree was added **without `--squash`** (full upstream history is in this repo), so pulls
must stay plain `git subtree pull` — never add `--squash`, which would realign history against a
different (squashed) upstream lineage. On a fresh clone the first pull recomputes the subtree
split of our local `genai/`-touching commits; it can take minutes and looks idle — let it run
(the result is cached afterwards).

Our additions live in new modules (`genai/src/{assistant,stream_fn,auth,codex,…}`), so
upstream merges mostly touch only `genai/src/lib.rs` and `genai/Cargo.toml`.

After every pull, before committing the merge result:

1. **Re‑derive the fork version.** If upstream bumped `genai/Cargo.toml`'s `version` (e.g. to
   `0.7.0-beta.20`), re‑apply our lineage scheme: `<upstream-version>.<n>-agentprism` (reset
   `<n>` to `1` on an upstream bump, increment it for fork‑only releases). Keep
   `repository`/`homepage` pointing at this repo.
2. **Reconcile the lockstep pin.** `agent/Cargo.toml`'s
   `genai = { package = "genai-agentprism", version = "=…", path = "../genai" }` must exact‑match
   the new fork version — the publish workflow's preflight fails the release otherwise.
3. **Check feature drift.** Compare upstream's `[features]` against ours; keep the folded
   `auth`/`loopback`/`codex`/`codex-auth-resolver`/`testing` features and the TLS
   mutual‑exclusion `compile_error!` intact, and mirror any genuinely new upstream feature in
   `[package.metadata.docs.rs]` if it should be documented.
4. **Never `cargo fmt` the subtree** (it keeps upstream's style; CI's fmt gate covers only
   `agent/` + `ffi/`).
5. **Re‑run the offline matrix** (the two `cargo test` commands from Build & test) and curate
   `genai/CHANGELOG.md` — upstream entries keep their `.`/`-`/`+`/`^`/`!` markers; add ours the
   same way.

## Staying current with pi-agent-core

`rust-genai-agent` is a port of `@earendil-works/pi-agent-core`, and the port **tracks the
latest pi releases** — it is not a one-time snapshot. The tracking mechanism is the parity
matrix in [`agent/tests/parity_manifest.toml`](agent/tests/parity_manifest.toml): it pins the
`earendil-works/pi` commit the matrix was last synced against (`upstream_commit`) and maps every
concrete vitest case in the four non-harness test files of `pi/packages/agent`
(`agent-loop.test.ts`, `agent.test.ts`, `e2e.test.ts`, `proxy.test.ts`) one-for-one to a Rust
parity test, each with a milestone and a `green`/`active`/`divergence` status. The gate is
`python3 agent/scripts/check_test_parity.py`: it re-reads the pinned upstream sources from the
`pi/` checkout beside the workspace (check the pi repo out at `genai-agent-rs/pi`, or symlink a
sibling checkout there — the path is gitignored), fails when their concrete case set has drifted
from the script's baseline (“update the parity baseline deliberately”), and verifies the
aggregate manifest, its ordered fragments, and every mapped Rust test name.

When a new pi-agent-core release lands upstream, the matrix is re-synced deliberately:

1. Fast-forward the `pi/` checkout to the new release commit.
2. Run `python3 agent/scripts/check_test_parity.py` — it reports the drifted case set by name.
3. Re-baseline the source counts in the script, bump `expected_cases` in the manifest, and set
   `upstream_commit` to the new pin.
4. Port every added, removed, or renamed case: implement the mapped Rust parity test, update the
   manifest and its ordered fragments (`agent/tests/parity/*.toml`), and keep every entry
   `green` — a `divergence` status needs a documented reason in
   [`agent/docs/parity-roadmap.md`](agent/docs/parity-roadmap.md).
5. Refresh the count claims in `agent/README.md` and the roadmap so docs and matrix never
   disagree.

All mapped cases must be green at release time; the roadmap's "Dropped" section records the
upstream behaviors deliberately not ported (no production consumer). The most recent sync
(pi `581d75a89…`) ported the `toolcall_end` metadata change and grew the matrix from 55 to
**56/56** mapped cases — see the agent [changelog](agent/CHANGELOG.md).

## Publishing

Releases run through **Actions → "Publish to crates.io"** (`workflow_dispatch`, dry‑run by
default) behind the `crates-io` environment approval. The workflow:

1. publishes the two crates in lockstep via crates.io Trusted Publishing (no stored token):
   `genai-agentprism` first, then `rust-genai-agent` (which exact‑pins it). Already‑published
   versions are skipped, so a dispatch with unchanged crates is a no‑op;
2. then the `apple` job releases the **Swift package** at the same version: rebuilds the
   XCFramework, rewrites the root distribution `Package.swift` (zip URL + checksum), commits
   it plus the regenerated UniFFI bindings, tags the bare semver (`0.2.0` — the Swift Package
   Index ignores the crate‑prefixed tags), and creates the GitHub Release holding the zip.

So bumping both crate versions and dispatching the workflow ships the Rust crates **and** the
Swift package in one gated release. Manual fallback (emergencies only):

```bash
cargo publish -p genai-agentprism      # pi-ai layer, first
cargo publish -p rust-genai-agent      # depends on it, second
ffi/release_swift.sh                 # Swift package (macOS only; honors --dry-run)
```

## License

MIT OR Apache‑2.0. `genai-agentprism` is a fork of
[`jeremychone/rust-genai`](https://github.com/jeremychone/rust-genai) (MIT OR
Apache‑2.0); the agent layer is a port of
[`@earendil-works/pi-agent-core`](https://github.com/earendil-works).
