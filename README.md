# genai-agent-rs

A Rust port of [pi](https://github.com/earendil-works)'s agent stack. The governing
statement is [`docs/porting-pi-ai-and-agent-core-docs/goal.md`](docs/porting-pi-ai-and-agent-core-docs/goal.md)
(mirrored verbatim in `AGENTS.md` and `CLAUDE.md`), and the adopted architecture is
[`architecture-v2-part1-proposal.md`](docs/porting-pi-ai-and-agent-core-docs/architecture-v2-part1-proposal.md)
+ [`architecture-v2-part2-revision.md`](docs/porting-pi-ai-and-agent-core-docs/architecture-v2-part2-revision.md).
In one line: **port pi's contracts and dependency boundaries — not its TypeScript — into idiomatic Rust**,
with parity defined operationally (parity manifest, conformance suites, byte-identical provider request
bodies, a divergence allowlist) and four commitment gates.

| pi package | crates (adopted architecture) | status |
|---|---|---|
| `@earendil-works/pi-ai` | `pi-ai` + provider crates + `pi-ai-providers-all` | to be built (Milestone 1 first) |
| `@earendil-works/pi-agent-core` | `pi-agent-core`, `pi-agent-session`, `pi-agent-harness`, `pi-agent-env`, `pi-agent-runtime-tokio`, `pi-agent-compat-pi-jsonl` | to be built |
| bindings | `pi-ffi` | to be built |

Reading order for anyone working on the port: `goal.md`, then the two architecture documents, then pi's
pinned source (`c49906ec7`), then the index in
[`docs/porting-pi-ai-and-agent-core-docs/README.md`](docs/porting-pi-ai-and-agent-core-docs/README.md).

## Legacy crates — `ai/`, `genai/`, `agent/`, `ffi/`

None of these is the port described by `goal.md`; they predate the adopted architecture and are not
baselines. They still build and are kept for their existing consumers and as a quarry.

- **`ai/`** — `agentprism-ai`, the earlier pi-ai port built to a retired standard (byte-observable
  parity with pi's JavaScript behavior). Its wire encoders, SSE decoders, OAuth flows, 430+ hermetic
  tests, and the TS-capture harness under `ai/examples/` are reusable material. Unreviewed remediation
  work from that standard is on the `wip/parity-remediation-p01` branch.

- **`genai/`** — `genai-agentprism`, an owned fork of
  [`jeremychone/rust-genai`](https://github.com/jeremychone/rust-genai) (synced via `git subtree`)
  carrying an assistant/stream contract, `GenaiStreamFn`, and feature-gated `auth`/`codex` modules
  used by the legacy agent crate. Lib target `genai`.
- **`agent/`** — `rust-genai-agent`, an earlier provider-neutral agent loop built on `genai`
  with a deliberately partial scope (no harness, sessions, compaction, …). The pi-agent-core port
  will be built on `ai` instead, to the full-parity standard.
- **`ffi/`** — `genai-agent-ffi`, UniFFI (Swift) bindings for `agent/` (see `docs/embedding.md`,
  `docs/using-from-swift.md`).

## Workspace layout

```
genai-agent-rs/
├── Cargo.toml          # [workspace] members = ["genai", "agent", "ffi", "ai"]
├── ai/                 # agentprism-ai   — legacy pi-ai port (retired standard; quarry)
├── genai/              # genai-agentprism — legacy rust-genai fork
├── agent/              # rust-genai-agent — legacy agent loop on genai
├── ffi/                # genai-agent-ffi  — Swift bindings for agent/
├── docs/               # goal.md + porting background; embedding/Swift docs
└── workflows/          # AgentPrism workflow scripts (archive/ holds retired-standard runs)
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

## Staying current with pi-agent-core (legacy `agent/` crate)

*This section describes the legacy `rust-genai-agent` crate's own tracking mechanism. It is not the parity standard for the pi-agent-core port on `ai` — that standard is `goal.md`.*

`rust-genai-agent` is an earlier, partial port of `@earendil-works/pi-agent-core` that **tracks
the latest pi releases** — it is not a one-time snapshot. The tracking mechanism is the parity
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
   the legacy crate's changelog.
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
Apache‑2.0); `agentprism-ai` is a port of
[`@earendil-works/pi-ai`](https://github.com/earendil-works) and the legacy agent layer is an
earlier partial port of `@earendil-works/pi-agent-core`.
