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
| `@earendil-works/pi-ai` | `pi-ai` + provider crates + `agentprism-providers-all` | to be built (Milestone 1 first) |
| `@earendil-works/agentprism-core` | `agentprism-core`, `agentprism-session`, `agentprism-harness`, `agentprism-env`, `agentprism-runtime-tokio`, `agentprism-compat-pi-jsonl` | to be built |
| bindings | `pi-ffi` | to be built |

Reading order for anyone working on the port: `goal.md`, then the two architecture documents, then pi's
pinned source (`c49906ec7`), then the index in
[`docs/porting-pi-ai-and-agent-core-docs/README.md`](docs/porting-pi-ai-and-agent-core-docs/README.md).

## Legacy crates

The earlier crates — `ai/` (the pi-ai port built to the retired byte-observable standard), `genai/`
(a rust-genai fork), `agent/` (an earlier partial agent loop), and `ffi/` (Swift bindings for it) —
live on the [`legacy/pre-architecture-v2`](https://github.com/agentprism/genai-agent-rs/tree/legacy/pre-architecture-v2)
branch with their docs. They are not baselines for the adopted architecture; `ai/` is a quarry for
wire encoders, SSE decoders, OAuth flows, hermetic tests, and the pi-request capture harness.
Unreviewed remediation work under the retired standard is on `wip/parity-remediation-p01`.

## Workspace layout

```
genai-agent-rs/
├── Cargo.toml                      # [workspace] — see members
├── crates/
│   ├── agentprism-ai/                      # canonical model, replay/stream, lowering, Models control plane
│   ├── agentprism-core/              # agent state machine over ModelRuntime
│   ├── agentprism-session/           # entry tree, lanes, operation records, reducers, storage traits
│   ├── agentprism-harness/           # compaction, skills, templates, reference tools, telemetry
│   ├── agentprism-env/               # filesystem/process capability traits
│   ├── agentprism-runtime-tokio/     # Tokio environment, Send actor facade, process execution
│   └── agentprism-compat-pi-jsonl/   # pi v4 JSONL reader + constrained writer
├── providers/                      # one crate per provider + agentprism-providers-all (Milestone 4+)
├── bindings/agentprism-ffi/                # opaque handles, versioned envelopes (Milestone 5)
├── parity/                         # parity manifest + checker (Milestone 1)
├── docs/porting-pi-ai-and-agent-core-docs/
└── workflows/                      # AgentPrism milestone workflow (archive/ holds retired runs)
```

## Build & test

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test  --workspace          # hermetic: ScriptedRuntime and captured fixtures only
bash parity/check.sh             # parity manifest (Part 2 §10), once Milestone 1 lands it
```

## Running a milestone

`workflows/architecture-v2-milestones.workflow.js` builds one milestone per run (`--args
'{"milestone":"M1"}'`; `M1`–`M9`, then `GATES`). Every package is implemented by a
`codex/gpt-5.6-sol` xhigh agent, reviewed by an independent one against the architecture
documents and pi's pinned source, and committed on approval. It requires a clean `main` and the
pi worktree at the pinned commit.

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

## Staying current with agentprism-core (legacy `agent/` crate)

*This section describes the legacy `rust-genai-agent` crate's own tracking mechanism. It is not the parity standard for the agentprism-core port on `ai` — that standard is `goal.md`.*

`rust-genai-agent` is an earlier, partial port of `@earendil-works/agentprism-core` that **tracks
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

When a new agentprism-core release lands upstream, the matrix is re-synced deliberately:

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

MIT OR Apache-2.0. The crates are a port of [`@earendil-works/pi`](https://github.com/earendil-works/pi)
(`packages/ai`, `packages/agent`) at the pinned commit named in `goal.md`.
