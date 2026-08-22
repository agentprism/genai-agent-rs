# Archived workflow scripts — completed one-off runs

These scripts ran (or were drafts for) the initial pi-ai port against pin `496185f6`. They are kept for provenance only:

- Their paths are macOS-specific and stale.
- Their "STANDING DIRECTIVE" blocks are superseded by `docs/porting-pi-ai-and-agent-core-docs/goal.md`.
- Several encode directives that **contradict** `goal.md` and must not be copied: the "partial-free event protocol / MessageBuilder replaces pi's carried `partial`" design (`pi-ai-port-openai-family` lines 29–33, 86–96; `pi-ai-full-port` 192–193; `pi-ai-fidelity-repair` 59, 62) and "binding seam rulings" / docs-as-authority framing (every `COMMON` block except `pi-ai-full-port-v2`).

What is worth carrying forward: the four-column CANNOT-PRESERVE protocol, and `pi-ai-full-port-v2`'s authority framing (pi source is authority; docs are context). The live workflows are in the parent folder.
