export const meta = {
  name: "boltffi-swift-design",
  description:
    "Produce the implementation-ready design for the lower-level Agent Swift SDK via BoltFFI. Grounded exclusively in the live BoltFFI documentation (verbatim snapshot, every claim cited) and governed by the two adopted owner reviews. codex/gpt-5.6-sol xhigh only.",
  model: "codex/gpt-5.6-sol",
  phases: [{ title: "Preflight" }, { title: "Corpus" }, { title: "Inventory" }, { title: "Design" }, { title: "Closeout" }],
};

const A = typeof args === "string" ? JSON.parse(args) : args || {};
const REPO = A.repo || "/home/vikash/genai-agent/genai-agent-rs-boltffi";
const BRANCH = A.branch || "boltffi-design";
const MAX_ROUNDS = A.maxRounds || 6;
const INITIAL_FEEDBACK = typeof A.initialFeedback === "string" && A.initialFeedback.trim() ? A.initialFeedback : null;
const MODEL = "codex/gpt-5.6-sol";
const XHIGH = { reasoning_effort: "xhigh" };
const OUT = "docs/boltffi-swift-bindings";
const SNAP = OUT + "/docs-snapshot";
const REVIEW1 = OUT + "/owner-review-2026-08-26.md";
const REVIEW2 = OUT + "/owner-review-2026-08-26b-implementation-audit.md";
const SEED_URLS = [
  "https://www.boltffi.dev/docs/overview.md",
  "https://www.boltffi.dev/docs/async.md",
  "https://www.boltffi.dev/docs/streaming.md",
  "https://www.boltffi.dev/docs/errors.md",
  "https://www.boltffi.dev/docs/custom-types.md",
  "https://www.boltffi.dev/docs/functions.md",
];

const COMMON =
  "Repository: " + REPO + " (branch " + BRANCH + ", a worktree of the main project at the agentprism-* crate names: crates/agentprism-ai, agentprism-core, agentprism-session, agentprism-harness, agentprism-env, agentprism-runtime-tokio; providers/agentprism-*; bindings/agentprism-ffi). You never modify crate source in this workflow; you produce documents under " + OUT + "/ only.\n\n" +
  "SCOPE (owner decision): this milestone designs the LOWER-LEVEL AGENT SWIFT SDK — the concrete Tokio actor boundary (TokioAgentHandle, TokioAgentRun, owned event/outcome/snapshot/control values, AgentEventSink) plus the direct model-stream path. The production coding-agent SDK over agentprism-harness is a separate future milestone and its absence here is not a gap.\n\n" +
  "GOVERNING REVIEWS: " + REVIEW1 + " (adopted 2026-08-26: corrected R2, lossless async-pull boundary, TokioAgentRun reshape, sink semantics, envelope option 2, Rust-owned runtime, consumer scope, 12 acceptance tests) and " + REVIEW2 + " (adopted 2026-08-26: implementation audit — direction retained, 16 blocking findings that make the plan implementation-ready). Both are authoritative; read both in full before any other work.\n\n" +
  "ABSOLUTE RULE — NO MEMORY-BASED BOLTFFI CLAIMS: model knowledge of BoltFFI is known to be outdated. Every assertion about what BoltFFI supports, requires, generates, or forbids MUST cite a page under https://www.boltffi.dev/docs/ together with the local snapshot path under " + SNAP + "/ containing the supporting text. If the documentation does not answer a question, write 'UNRESOLVED: not answered by the documentation' and list the pages checked; the Phase-0 probe (audit finding 13) is the designated mechanism for resolving UNRESOLVED items empirically — never model memory, blogs, or third-party sources. Where an owner review asserts BoltFFI behavior (including the 0.30.1 source-behavior claims in the audit), re-verify against the snapshot and cite, or carry it as a Phase-0 probe item; flag any discrepancy prominently.\n\n" +
  "REQUIREMENTS:\n" +
  "R1. The bindings expose the API an ordinary Rust application consumes at the scope boundary above — not a hand-rolled envelope layer.\n" +
  "R2. Integration must not introduce a separately maintained FFI facade, duplicate record hierarchy, IDL, or required Swift wrapper. Existing canonical crates may receive inline BoltFFI annotations and minimal concrete API changes needed to project their ordinary consumer contracts safely — owned returns, concrete collection inputs, interior synchronization, async pull methods, and Rust-owned runtime integration. Every such change must remain a legitimate Rust API rather than a binding-only command/envelope layer.\n" +
  "R3. Async and streaming are the heart of the library; async.md and streaming.md govern those mappings; authoritative AgentEvent/AssistantEvent delivery is lossless async pull, never a drop-on-full EventSubscription.\n\n" +
  "Ways of working: cite our code as file:line against the CURRENT tree (agentprism-* names); keep documents in Markdown under " + OUT + "/; do not run cargo publish; git commits are made only by the commit step. Report only what you actually observed.";

const REVIEW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "feedback", "summary", "citationsChecked", "blocking"],
  properties: {
    ok: { type: "boolean", description: "true only if the phase deliverable is complete, review-conformant, and every BoltFFI claim is correctly cited" },
    feedback: { type: "string", description: "Specific feedback for the next round; empty when ok" },
    summary: { type: "string", description: "Up to five lines: what was verified and how" },
    citationsChecked: {
      type: "array",
      description: "Citations you verified (claim → URL → verdict)",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["claim", "url", "verdict"],
        properties: {
          claim: { type: "string", description: "The claim, abbreviated" },
          url: { type: "string", description: "The cited boltffi.dev URL" },
          verdict: { type: "string", enum: ["supported", "unsupported", "page-does-not-say-this"], description: "Whether the cited page actually supports the claim" },
        },
      },
    },
    blocking: {
      type: "array",
      description: "Blocking defects",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["where", "issue"],
        properties: { where: { type: "string", description: "file/section" }, issue: { type: "string", description: "What is wrong" } },
      },
    },
  },
};

const COMMIT_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["committed", "sha", "note"],
  properties: {
    committed: { type: "boolean", description: "Whether a commit was created" },
    sha: { type: "string", description: "git rev-parse HEAD after the commit, or empty" },
    note: { type: "string", description: "One-line summary or why nothing was committed" },
  },
};

async function build(id, implText, reviewText, commitMsg) {
  const outcome = await gate(
    (feedback, attempt) =>
      agent(
        implText + ((feedback = feedback || (attempt === 0 && id === "Design" ? INITIAL_FEEDBACK : null)) ? "\n\nA REVIEWER REJECTED the previous attempt (round " + attempt + "). Your files are still on disk; fix every point without discarding what was correct:\n" + feedback : ""),
        { label: "impl:" + id + ":r" + (attempt + 1), phase: id, model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: XHIGH, retries: 1 }
      ),
    (result) =>
      agent(reviewText + "\n\nThe author reported:\n" + (result == null ? "(no report)" : String(result)), {
        label: "review:" + id, phase: id, model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: XHIGH, schema: REVIEW_SCHEMA, retries: 1,
      }),
    { attempts: MAX_ROUNDS }
  );
  if (!outcome.ok) return { id, approved: false, rounds: outcome.attempts, sha: null, verdict: outcome.verdict || {} };
  const commit = await agent(
    "In " + REPO + ": git add " + OUT + " && git commit -m '" + commitMsg + "'. Then report git rev-parse HEAD. Do not modify files, amend, push, or touch other branches.",
    { label: "commit:" + id, phase: id, model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: { reasoning_effort: "low" }, schema: COMMIT_SCHEMA }
  );
  const sha = commit && commit.committed === true && /^[0-9a-f]{7,40}$/i.test(String(commit.sha).trim()) ? String(commit.sha).trim() : null;
  log("✔ " + id + " approved after " + outcome.attempts + " round(s)" + (sha ? " · committed " + sha.slice(0, 9) : " · ⚠ no commit: " + (commit ? commit.note : "null")));
  return { id, approved: true, rounds: outcome.attempts, sha, verdict: outcome.verdict || {} };
}

phase("Preflight");
const pf = await agent(
  "Preflight only; change nothing. In " + REPO + ": report `git branch --show-current` and whether `git status --porcelain` is empty; confirm crates/agentprism-core exists (the tree is post-rename); confirm https://www.boltffi.dev/docs/overview.md is reachable (fetch it; report HTTP status and first heading). ok=true only if branch is '" + BRANCH + "', the tree is clean, the renamed crates exist, and the page fetched.",
  {
    label: "preflight", phase: "Preflight", model: MODEL, mode: "agent", cwd: REPO, configOptions: { reasoning_effort: "low" },
    schema: {
      type: "object", additionalProperties: false, required: ["ok", "branch", "clean", "renamed", "docsReachable", "note"],
      properties: {
        ok: { type: "boolean", description: "All conditions hold" },
        branch: { type: "string", description: "current branch" },
        clean: { type: "boolean", description: "porcelain empty" },
        renamed: { type: "boolean", description: "crates/agentprism-core exists" },
        docsReachable: { type: "boolean", description: "overview.md fetched" },
        note: { type: "string", description: "anything off" },
      },
    },
  }
);
if (!pf || !pf.ok) { log("✘ preflight failed: " + JSON.stringify(pf)); return { ok: false, preflight: pf }; }

phase("Corpus");
const corpus = await build(
  "Corpus",
  "You are the CORPUS agent (" + MODEL + ", xhigh). " + COMMON + "\n\nTask: refresh the verbatim BoltFFI documentation snapshot under " + SNAP + "/ (a prior snapshot exists on disk; replace stale pages, add new ones, never edit content).\n" +
    "1. Fetch the seed pages: " + SEED_URLS.join(" , ") + " and every other /docs/ page discoverable from them (navigation, inline links, index/sitemap/llms.txt), recursively to closure.\n" +
    "2. Save each page byte-for-byte under " + SNAP + "/<path>.md; update " + SNAP + "/MANIFEST.md (fetch date, URL → file → first heading → bytes; unreachable links with errors).\n" +
    "3. Record the BoltFFI VERSION the documentation describes wherever the docs state one (release notes, installation page, changelog); the audit requires an exact version pin (0.30.1 was the audited source behavior) — capture whatever the docs declare and note where.\n" +
    "Report: pages captured/refreshed, discovery method, the version evidence found, anything unreachable.",
  "You are the CORPUS REVIEWER (" + MODEL + ", xhigh), a fresh session. " + COMMON + "\n\nVerify the snapshot: seeds captured; re-fetch at least 5 pages live and diff; crawl the /docs/ links yourself and confirm none missing from the manifest; version evidence recorded truthfully (or its absence stated). ok=false with specifics otherwise.",
  "boltffi design v3: refreshed documentation snapshot + version evidence"
);
if (!corpus.approved) { log("✘ Corpus not approved — halting"); return { ok: false, phase: "Corpus", corpus }; }

phase("Inventory");
const inventory = await build(
  "Inventory",
  "You are the INVENTORY agent (" + MODEL + ", xhigh). " + COMMON + "\n\nTask: revise " + OUT + "/api-inventory.md against the CURRENT renamed tree (a stale pre-rename revision is on disk).\n" +
    "1. Re-cite everything at current paths: crates/agentprism-core (exports, examples, tests), crates/agentprism-runtime-tokio (TokioAgentHandle, TokioAgentRun, AgentEventSink, drive_run/dispatch_event, accept_run/idle machinery — the audit's race findings cite these; capture their CURRENT shapes precisely), the agentprism-ai seams (ModelRuntime, Models, streams, messages, tools, CancellationToken, DeferredHandle), bindings/agentprism-ffi/examples/scripted_host.rs.\n" +
    "2. Keep the classification (core/extended/excluded per the scope decision — harness surfaces are out of scope by owner decision, list them once as such) and the FFI-hard spots.\n" +
    "3. Add an audit-support appendix: the exact current code of accept_run idle handling, TokioAgentRun fields/methods, prompt_text_with_sink, envelope/sequence allocation in the actor, and TokioAgentHandle::new runtime acquisition — with file:line — so the Design phase can address audit findings 2–8 and 16 against ground truth.\n" +
    "No BoltFFI claims in this phase.",
  "You are the INVENTORY REVIEWER (" + MODEL + ", xhigh), a fresh session. " + COMMON + "\n\nVerify every citation resolves at the CURRENT tree (any surviving crates/pi-* path is an automatic rejection); the audit-support appendix quotes the real current code for accept_run, TokioAgentRun, sink runs, envelope allocation, and runtime acquisition; the core set still covers an end-to-end run; no BoltFFI claims present.",
  "boltffi design v3: inventory re-cited against the renamed tree + audit-support appendix"
);
if (!inventory.approved) { log("✘ Inventory not approved — halting"); return { ok: false, phase: "Inventory", inventory }; }

phase("Design");
const design = await build(
  "Design",
  "You are the DESIGN agent (" + MODEL + ", xhigh). " + COMMON + "\n\nTask: revise " + OUT + "/design.md into an IMPLEMENTATION-READY blueprint. The prior revision's direction is ratified (lossless pull, actor boundary, run reshape, sink distinction, envelope option 2, Rust-owned runtime) — retain it. What was rejected is implementation readiness: address ALL 16 findings of " + REVIEW2 + ", each in an explicit, numbered subsection that quotes the finding it resolves:\n" +
    "F1 stale names — the whole document cites the current agentprism-* tree.\n" +
    "F2 accept_run check-to-send race — adopt the audit's send-then-restore fix and its deterministic test.\n" +
    "F3 established-output drop policy for TokioAgentRun AND TokioAssistantStream (choose and specify one of the three options; add the post-ready/pre-retain test).\n" +
    "F4 assistant-stream drop must release the runtime lease (Drop-or-lease cancellation; provider-pending-forever teardown test).\n" +
    "F5 sink-only naming — no silent semantic change; specify prompt_text_with_sink (pull + sink, unchanged) and prompt_text_sink_only (no observational sender), or an explicit observation mode.\n" +
    "F6 EOF state model — separate producer terminal validation from consumer terminal delivery; specify Ok(None) rules for active-pull vs closed/never-installed observation.\n" +
    "F7 concurrent pulls — reject with TokioAgentError::ConcurrentEventPoll (the audit's preference) unless you justify serialization with uniqueness/sequence-completeness tests instead.\n" +
    "F8 envelope SnapshotInvariant gap — pick and fully specify one resolution (core-allocated envelopes preferred if coherent) plus the forced-invariant/next-run sequence test.\n" +
    "F9 root library — name the canonical BoltFFI root crate, its dependency graph, staticlib/build.rs/boltffi.toml ownership, and how runtime+provider symbols are reachable without cycles.\n" +
    "F10 xtask package-apple — the full repository-owned pipeline (exact BoltFFI version pin from the corpus evidence or 0.30.1 verified, generator/source compat check, completeness check, slices, XCFramework, SwiftPM, XCTest, naming, clean-checkout reproducibility).\n" +
    "F11 Swift 6 Sendable — a generator-owned resolution, tested under strict concurrency; no required handwritten wrapper.\n" +
    "F12 generated-surface completeness gate — manifest/contract check with #[skip]-plus-reason discipline.\n" +
    "F13 implementation order — Phase 0 disposable capability probe (multi-crate scanning, cfg_attr, tuples, nested errors, owned class callback args, non-exhaustive enums, value graph, Swift 6) then the audit's eight slices.\n" +
    "F14 provider-neutral transport crate (e.g. agentprism-transport-reqwest); provider leaves keep Arc<dyn HttpTransport>.\n" +
    "F15 provider-neutral native Models factory preserving the control plane (persistent credentials, check_auth/login, codex device-code OAuth) with a captured no-live-network acceptance path for openai-codex / gpt-5.6-sol / ReasoningLevel::Xhigh; API-key OpenAI must not be the sole construction path.\n" +
    "F16 split TokioRuntimeOwner (executor/supervisor only) from TokioAgentFactory (Models + ToolRegistry + spawning), or rename and document the combined type as a factory.\n" +
    "Also: state the scope decision (option 1) in §1; keep both reviews' earlier requirements (12 acceptance tests, extended per audit findings 2–8's new tests); every BoltFFI claim cited [URL | snapshot-path]; UNRESOLVED items each assigned to the Phase-0 probe.",
  "You are the DESIGN REVIEWER (" + MODEL + ", xhigh), a fresh session — the conformance and citation auditor. " + COMMON + "\n\nAdversarially verify design.md:\n" +
    "1. AUDIT CONFORMANCE: walk " + REVIEW2 + " findings 1–16 one by one; each must be resolved as specified (or with an explicitly justified, equally strong alternative where the audit offered options); a watered-down or missing finding is a rejection. The ratified direction from " + REVIEW1 + " must be retained.\n" +
    "2. CITATIONS: every BoltFFI claim carries [URL | snapshot-path] and the cited text supports it; re-fetch at least 10 live; audit-asserted BoltFFI behaviors are re-verified or assigned to Phase 0, never repeated on authority.\n" +
    "3. CODE TRUTH: our-code claims verified at current file:line (crates/agentprism-*); the race analyses match the inventory appendix's quoted code.\n" +
    "4. CANONICAL-API DISCIPLINE: every proposed Rust change remains a legitimate ordinary Rust API; out-of-scope seams stay unannotated; no silent semantic change to existing methods (F5).\n" +
    "Reject with file/section-specific feedback.",
  "boltffi design v3: implementation-ready blueprint under both owner reviews"
);

phase("Closeout");
const ok = design.approved;
log((ok ? "✔" : "✘") + " boltffi design v3 " + (ok ? "complete" : "NOT approved") + " — corpus " + corpus.rounds + "r, inventory " + inventory.rounds + "r, design " + design.rounds + "r");
return { ok, corpus, inventory, design, out: OUT };
