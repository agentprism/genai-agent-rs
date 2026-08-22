# Porting pi-ai and pi-agent-core — document index

| Document | Role |
|---|---|
| [`goal.md`](goal.md) | **Governing statement.** What the port is and the standard it is held to. Mirrored verbatim in the repo's `AGENTS.md` and `CLAUDE.md`. |
| `parity-audit-<date>.md` | The current parity audit of the `ai` crate against `goal.md` (produced by `workflows/pi-ai-parity-audit.workflow.js`), with its remediation plan and per-finding status. |
| [`v2/preserved-architectural-seams-pi-ai-v2.mdx`](v2/preserved-architectural-seams-pi-ai-v2.mdx) | Background: architectural seams of pi-ai worth preserving. Not authority; corrected where it had contradicted pi. |
| [`v2/preserved-architectural-seams-pi-agent-core-v2.mdx`](v2/preserved-architectural-seams-pi-agent-core-v2.mdx) | Background for the pi-agent-core port (built on `ai`). Not authority; corrected where it had contradicted pi. |
| [`provider-api-implementations.mdx`](provider-api-implementations.mdx) | Background: pi's per-API transport behavior, read from pi source; its scope/transport framing is historical. |
| [`openai-family-port-independent-audit-2026-08-20.md`](openai-family-port-independent-audit-2026-08-20.md) | Archived historical audit; all findings resolved or superseded. |

Authority order: pi's pinned source (see the pin in `ai/src/lib.rs`) → `goal.md` → nothing else. Any document here that disagrees with pi is wrong.

Completed one-off workflow scripts from the initial port live in `workflows/archive/` and must not be copied as policy.
