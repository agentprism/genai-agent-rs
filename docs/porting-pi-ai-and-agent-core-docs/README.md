# Porting pi-ai and pi-agent-core — document index

| Document | Role |
|---|---|
| [`goal.md`](goal.md) | **Governing statement.** What the port is and the standard it is held to. Mirrored verbatim in the repo's `AGENTS.md` and `CLAUDE.md`. |
| [`architecture-v2-part1-proposal.md`](architecture-v2-part1-proposal.md) | **Adopted architecture, part 1** — crate layout, `ModelRuntime`/`Models` seam, data model, streaming, options, providers, catalogs, auth, agent state machine, tools, policies, FFI, errors, testing, milestones. |
| [`architecture-v2-part2-revision.md`](architecture-v2-part2-revision.md) | **Adopted architecture, part 2** — source-pinned revision (takes precedence over part 1): replay envelope and stream protocol, failure/retry/middleware, option lowering, handoff policy, catalog model, auth interaction and FFI, harness crates, pi-agent-core surface mapping, runtime model, **§10 operational definition of parity**, **§10.11 divergence allowlist**, commitment gates. |
| [`parity-audit-2026-08-21.md`](parity-audit-2026-08-21.md) | Historical: audit of the `ai/` crate under the *previous* (retired) standard. Useful as a map of pi behaviors; its remediation plan is void. |
| [`v2/preserved-architectural-seams-pi-ai-v2.mdx`](v2/preserved-architectural-seams-pi-ai-v2.mdx) | Background: earlier seam notes for pi-ai. Superseded by the architecture documents. |
| [`v2/preserved-architectural-seams-pi-agent-core-v2.mdx`](v2/preserved-architectural-seams-pi-agent-core-v2.mdx) | Background: earlier seam notes for pi-agent-core. Superseded by the architecture documents. |
| [`provider-api-implementations.mdx`](provider-api-implementations.mdx) | Background: pi's per-API transport behavior, read from pi source. |
| [`openai-family-port-independent-audit-2026-08-20.md`](openai-family-port-independent-audit-2026-08-20.md) | Archived historical audit under the retired standard. |

Authority order: the architecture documents for shape; pi's pinned source (`c49906ec7`, see `goal.md`) for behavior; the parity manifest and conformance suites for what "done" means. The `ai/`, `genai/`, `agent/`, and `ffi/` crates predate the adopted architecture and are not baselines.

Completed one-off workflow scripts from the earlier port live in `workflows/archive/` and must not be copied as policy.
