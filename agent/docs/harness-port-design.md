# Harness port design study (hypothetical — out of scope by design)

> **Legacy crate.** This documents `rust-genai-agent`, an earlier, deliberately partial agent loop built on the `genai` fork. It is **not** the pi-agent-core port defined by `docs/porting-pi-ai-and-agent-core-docs/goal.md`, which will be built on the `ai` crate to a full-parity standard; its scope exclusions, genai-foundation gap tables, and version pins are not that port's standard and may be stale. pi's pinned source — not this document — is authority.

The TS package's `src/harness/**` and `src/node.ts` were **deliberately excluded** from this crate
(see README). This document records the design study for how the harness *would* be faithfully
ported if it were ever brought into scope. It condenses the 2026-08-06 investigation; it is a
plan, not a commitment. Note: the study predates the current pi parity pin
(`581d75a89`); use the separate `rust-genai-agent-harness` crate for the landed harness
substrate and perform a fresh upstream survey diff before extending that port.

## The critical structural finding

Upstream `agent-harness.ts` is a **scaffold, not a finished orchestrator**: every run-path method
(`prompt`, `compact`, `resume`, `navigateTree`, …) rejects with `HarnessNotImplemented`, and
`create()` refuses to open any session that already contains records. What is finished — and
heavily tested — is the substrate: the durable session model, JSONL v4 store, record-log
validator/reducer, compaction, reference tools, execution env, and skills. A faithful port
therefore targets (a) the substrate at full fidelity and (b) the scaffold's type surface (error
taxonomy, outcome types, defaults, `HarnessNotImplemented`/`HarnessClosed` semantics). Inventing a
run loop would exceed "faithful."

## Proposed crate

`rust-genai-agent-harness`, workspace sibling depending on this crate and `genai`, mirroring the
TS layering:

```
src/
  error.rs types.rs                       # result.ts + tagged errors; capability traits
  env/{mod,native}.rs                     # FileSystem/Shell traits; NodeExecutionEnv port  [feature: native-env]
  messages.rs                             # HarnessMessage wire enum + convert_to_llm
  session/{types,state,session,context,memory,search}.rs
  session/jsonl/{codec,storage,repo,errors,types}.rs      [feature: jsonl (default)]
  session/testing.rs                      # ported conformance suite                        [feature: testing]
  reducer.rs
  compaction/{compaction,branch_summarization,utils}.rs
  util/{truncate,shell_output}.rs
  tools/{bash,read,write,edit,edit_diff,image,path_utils,file_mutation_queue,tool_context}.rs  [feature: tools]
  skills.rs prompt_templates.rs system_prompt.rs
  telemetry.rs                                            [feature: telemetry]
  agent_harness.rs                        # scaffold surface
```

New deps beyond this crate's set: `uuid` (v7), a YAML 1.2 parser, `ignore`, `similar` (diffs),
`tempfile`, `libc`/`nix` (unix, native-env only), optional `image` (the `ReadImageProcessor`
implementation only — core sniffing is hand-rolled bytes and dependency-free).

## Module notes and key behavioral contracts (must-preserve)

- **messages**: exact summary wrapper strings (`COMPACTION_SUMMARY_PREFIX/SUFFIX`, branch
  variants), `bashExecutionToText` format, `convertToLlm` projection rules. TS extends the open
  `AgentMessage` union by declaration merging; Rust models it as a closed `HarnessMessage` enum
  with an `#[serde(untagged)] Unknown(Value)` tail (open-union escape hatch).
- **session core**: entry tree (7 entry types) + lane-scoped record log (9 record types) sharing
  one monotonic `seq`; `applyMutation` invariants (seq = sequence+1, duplicate-id rejection across
  entries *and* records, lane-leaf chaining, exact stats accumulation); one open operation per
  lane; fork semantics (branch forks target message entries only, re-sequenced from 1, main lane
  pointer only); `assertJsonSerializable` payload hygiene; context building (latest compaction
  becomes the head; deferred assistants excluded; compaction entries expand to summary +
  `retainedTail`).
- **jsonl**: v4 header (version literal 4, `parentSessionId` xor `legacyParentSessionPath`);
  strict decode whitelists; **torn-tail repair only on JSON-syntax failure of the final line** — a
  semantically invalid but well-formed final line rejects the whole file (encode as
  `ParseFailure::{Syntax,Semantic}`; easy to get subtly wrong); atomic `.tmp` + rename staging;
  single-writer append queue whose failed op neither poisons the queue nor advances state; cwd-
  encoded directory naming and `{ISO-with-[:.]→-}_{id}.jsonl` file naming.
- **reducer**: pure functions; 12 verbatim corruption reasons; step-attempt series rules;
  tool-start invocation keys; queue-cancellation rules; provisioned-entry deep-equality ignoring
  `parentId/seq/timestamp`; `overflowRecoveryUsed` and terminal-failure derivations. Best-fitting
  module for Rust — port as borrowed-slice functions.
- **compaction**: defaults `{enabled: true, reserveTokens: 16384, keepRecentTokens: 20000}`;
  chars/4 estimation with images = 4800 chars (**the heuristic's exact values are the contract —
  do not substitute a real tokenizer**); cut-point selection/split-turn rules; verbatim
  summarization prompt texts; summarization request hygiene (`cacheRetention: "none"`, fresh
  session id, maxTokens formulas). Needs a small drain-to-completion adapter over `StreamFn` plus
  a `RetryPolicy` port (prerequisite deltas in this crate).
- **utils**: truncation limits (2000 lines / 50 KiB / 500-char grep lines), `TruncationResult`
  field set (persisted in tool details = wire format), UTF-8-boundary tail slicing; shell capture
  sanitization, rolling tail buffer, lazy full-output temp file.
- **env/native**: errno→`FileErrorCode` mapping table; shell selection ladder (incl. Git
  Bash/WSL on Windows); POSIX process groups + kill(-pid); the **100 ms post-exit stdio grace
  state machine** (behaviorally observable); result precedence `callback_error > timeout >
  aborted > exit`; non-zero exit is an *ok* result. Highest platform risk; honest fidelity needs
  Windows CI.
- **tools**: schema constants must match the TypeBox JSON byte-for-byte (descriptions are
  model-visible); bash truncation notices and failure texts verbatim; read-tool offset/continuation
  notices; edit-diff fuzzy normalization + uniqueness/overlap rules + display-diff format;
  file-mutation queue serialization scoped per env (registry lives in the tool context, replacing
  TS's WeakMap). Diff caveat: `similar` differs from jsdiff at hunk boundaries — port jsdiff's
  line diff if byte-identical patches matter.
- **skills/templates/system-prompt**: SKILL.md short-circuit recursion, gitignore semantics via
  the `ignore` crate (verify against TS fixtures; the TS code string-prefixes patterns to the walk
  root), name/description validation (description-less skills are dropped), verbatim
  `formatSkillInvocation` / `<available_skills>` XML shapes, `$1..$n`/`${@:N:L}` substitution.
- **telemetry**: keep both schemas as serializable *data* (preserves the doc-generation check);
  emit via `tracing` with dotted attribute names; typed per-span structs (optionally generated by
  a proc-macro from the schema data). Lost: pi-telemetry's type-level schema inference and
  excess-key rejection ergonomics.
- **scaffold**: 14 tagged rejection errors, outcome unions, defaults, defensive-copy getters,
  `Err(NotImplemented{operation})` — a recoverable error, never `unimplemented!()` panics.

## JSONL compatibility verdict

**Yes — same format, same directories, interoperable files; target line-level JSON-semantic
equality, not byte identity** (TS itself does not canonicalize key order, and JS/serde float
formatting differs at extremes). Requirements: exact camelCase field names (incl. `followUp`),
exact snake_case tag values, **omit-not-null optionals** (`skip_serializing_if` everywhere — a
stray `"details":null` breaks the reducer's deep-equality), integer bounds checks on parse,
version-4 literal enforcement, `#[serde(flatten)]` unknown-field preservation on entries/records.
Proof strategy: golden TS-written sessions checked into the Rust repo, Rust-written sessions
parsed by the TS loader in CI, and the ported ~1000-line conformance suite run against both the
in-memory and jsonl backends.

## Fidelity hard spots (closest-faithful answers)

Declaration-merged open unions → enum + untagged tail; pi-telemetry type machinery → data +
generated structs; WeakMap-keyed queues → context-owned registry; deferred responses →
`DeferredHandle` as an opaque serde struct so reducer/JSONL stay faithful while *acting* on
deferrals stays with the (unimplemented anyway) orchestrator; UTF-16 `.length` semantics in token
estimation → `encode_utf16().count()` for cross-implementation determinism on shared sessions.

## Effort and order

Roughly **10–14 focused engineer-weeks** including interop/conformance infrastructure, dominated
(~40%) by the session core. Phases: (0) foundations (error/types/env traits/wire enum) →
(1) durable core (session + conformance suite → jsonl + TS↔Rust goldens → reducer) →
(2) environment (utils → native env, parallelizable with 1) → (3) capabilities (tools ∥
skills/templates ∥ compaction) → (4) telemetry + scaffold + polish. Highest-leverage early
investment: port the conformance suite and stand up the golden-file CI *before* writing the jsonl
backend, so fidelity is measured rather than asserted.

## Prerequisite deltas in rust-genai-agent (small)

A drain-to-`AssistantMessage` helper over `StreamFn`; a `RetryPolicy` port; a `DeferredHandle` /
`"deferred"` stop-reason representation in (or alongside) the harness wire types; re-export
hygiene so the harness does not duplicate `ThinkingLevel`/`QueueMode`.
