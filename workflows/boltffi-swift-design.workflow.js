export const meta = {
  name: "boltffi-swift-design",
  description:
    "Map the agent crates' ordinary-Rust consumer API to BoltFFI-generated Swift bindings. Research is grounded exclusively in the live BoltFFI documentation at boltffi.dev — a verbatim snapshot is captured first, and every capability claim in the design must cite a documentation URL; nothing is asserted from model memory. codex/gpt-5.6-sol xhigh only.",
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
const SEED_URLS = [
  "https://www.boltffi.dev/docs/overview.md",
  "https://www.boltffi.dev/docs/async.md",
  "https://www.boltffi.dev/docs/streaming.md",
  "https://www.boltffi.dev/docs/errors.md",
  "https://www.boltffi.dev/docs/custom-types.md",
  "https://www.boltffi.dev/docs/functions.md",
];

const COMMON =
  "Repository: " + REPO + " (branch " + BRANCH + ", a worktree of the main project; the milestone build continues elsewhere — you never touch crate source code in this workflow; you produce documents under " + OUT + "/ only).\n\n" +
  "ABSOLUTE RULE — NO MEMORY-BASED BOLTFFI CLAIMS: model knowledge of BoltFFI is known to be outdated and has caused wrong designs before. Every single assertion about what BoltFFI supports, requires, generates, or forbids MUST carry a citation to a page under https://www.boltffi.dev/docs/ (canonical URL) together with the local snapshot path under " + SNAP + "/ that contains the supporting text. If the documentation does not answer a question, write 'UNRESOLVED: not answered by the documentation' and list the pages checked — never fill the gap from memory or from third-party sources. Blog posts, GitHub issues, or crates.io READMEs are not acceptable sources.\n\n" +
  "PROJECT REQUIREMENTS the design must satisfy (verify each against the docs; where the docs contradict a requirement, flag it prominently rather than silently adapting):\n" +
  "R1. The bindings expose the API an ordinary Rust application consumes when using the agent crates — the same types, functions, streams, and traits a native Rust consumer uses (pi-agent-core and the pi-ai seams it re-exposes, plus the Tokio handle) — not a hand-rolled envelope layer.\n" +
  "R2. Integration must require no code changes to the crates beyond BoltFFI attributes on existing items (and any feature-gating of those attributes). If the documentation shows some surface cannot be exposed attribute-only, record it as a gap with the documented alternative.\n" +
  "R3. Async and streaming are the heart of this library (agent runs are async and yield event streams; model calls yield assistant-event streams) — the async.md and streaming.md pages govern those mappings and must be read in full.\n" +
  "Target language: Swift. The crates will shortly be renamed to the agentprism-* family (pi-ai → agentprism-ai, pi-agent-core → agentprism-core, etc.); write the design using current names with one note about the rename.\n\n" +
  "Ways of working: cite our code as file:line; keep documents in plain Markdown under " + OUT + "/; do not run cargo publish, do not modify anything outside " + OUT + "/; git commits are made only by the commit step, not by you. Report only what you actually observed.";

const REVIEW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "feedback", "summary", "citationsChecked", "blocking"],
  properties: {
    ok: { type: "boolean", description: "true only if the phase deliverable is complete and every BoltFFI claim is correctly cited" },
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
  "Preflight only; change nothing. In " + REPO + ": report `git branch --show-current` and whether `git status --porcelain` is empty, and confirm https://www.boltffi.dev/docs/overview.md is reachable (fetch it; report the HTTP status and its first heading). ok=true only if branch is '" + BRANCH + "', the tree is clean, and the page fetched.",
  {
    label: "preflight", phase: "Preflight", model: MODEL, mode: "agent", cwd: REPO, configOptions: { reasoning_effort: "low" },
    schema: {
      type: "object", additionalProperties: false, required: ["ok", "branch", "clean", "docsReachable", "note"],
      properties: {
        ok: { type: "boolean", description: "All three conditions hold" },
        branch: { type: "string", description: "current branch" },
        clean: { type: "boolean", description: "porcelain empty" },
        docsReachable: { type: "boolean", description: "overview.md fetched successfully" },
        note: { type: "string", description: "anything off, incl. first heading of overview.md" },
      },
    },
  }
);
if (!pf || !pf.ok) { log("✘ preflight failed: " + JSON.stringify(pf)); return { ok: false, preflight: pf }; }

phase("Corpus");
const corpus = await build(
  "Corpus",
  "You are the CORPUS agent (" + MODEL + ", xhigh). " + COMMON + "\n\nTask: capture a complete, verbatim snapshot of the BoltFFI documentation.\n" +
    "1. Fetch these seed pages: " + SEED_URLS.join(" , ") + "\n" +
    "2. Discover every other documentation page: follow every link to /docs/ paths found in the seed pages (navigation, inline links, 'next' links), recursively, until closure. If the site exposes an index/sitemap or llms.txt, use it and say so.\n" +
    "3. Save each page verbatim (byte-for-byte as fetched) under " + SNAP + "/<path-under-docs>.md. No editing, no reformatting.\n" +
    "4. Write " + SNAP + "/MANIFEST.md: fetch date, and one row per page — canonical URL, local file, first heading, byte size. List any /docs/ link that could not be fetched with its error.\n" +
    "Report: pages captured, discovery method, anything unreachable.",
  "You are the CORPUS REVIEWER (" + MODEL + ", xhigh), a fresh session. " + COMMON + "\n\nVerify the snapshot: (1) every seed URL is captured; (2) re-fetch at least 5 captured pages live and diff against the snapshot — byte-identical or trivially-volatile differences only; (3) crawl the seed pages' /docs/ links yourself and confirm none is missing from the manifest; (4) manifest rows are accurate. ok=false with specifics if any page is missing, edited, or misfiled.",
  "boltffi design: verbatim documentation snapshot + manifest"
);
if (!corpus.approved) { log("✘ Corpus not approved — halting"); return { ok: false, phase: "Corpus", corpus }; }

phase("Inventory");
const inventory = await build(
  "Inventory",
  "You are the INVENTORY agent (" + MODEL + ", xhigh). " + COMMON + "\n\nTask: write " + OUT + "/api-inventory.md — the API surface an ordinary Rust application consumes when using the agent crates (requirement R1). Work from the code, not from wishes:\n" +
    "1. Read crates/pi-agent-core/src/lib.rs exports and the examples/tests that exercise them; crates/pi-agent-runtime-tokio (TokioAgentHandle — the natural app-facing handle); the pi-ai seams a consumer touches through the agent (ModelRuntime, Models construction, AssistantEvent/AgentEvent, messages/content blocks, ToolSpec/Tool/TypedTool, CancellationToken, SessionStorage traits, DeferredHandle); bindings/pi-ffi/examples/scripted_host.rs as an existing consumer.\n" +
    "2. For every surface element record: item path, file:line, kind (async fn / stream / trait a host implements / trait we implement / data enum / struct / generic item / error type), Send-vs-Local family, and whether an ordinary embedding app needs it (core), may need it (extended), or is internal-leaking (exclude, say why).\n" +
    "3. Call out the FFI-hard spots explicitly: generic items (TypedTool<I,F>), trait objects crossing the boundary in both directions (host-implemented Tool/SessionStorage vs library-implemented ModelRuntime), the two boxed-stream families, CancellationToken, and large data enums (AssistantEvent, AgentEvent, ContentBlock).\n" +
    "This phase makes NO BoltFFI claims at all — it is a pure Rust-side inventory.",
  "You are the INVENTORY REVIEWER (" + MODEL + ", xhigh), a fresh session. " + COMMON + "\n\nVerify against the code: every listed item exists at its cited file:line; the core set is genuinely sufficient to run an agent end-to-end (walk bindings/pi-ffi/examples/scripted_host.rs and the pi-agent-core examples/tests and check nothing they use is missing); the FFI-hard spots section covers generics, bidirectional traits, both stream families, cancellation, and the big enums. Reject with specifics if items are missing, misclassified, or cited wrongly. This phase must contain no BoltFFI claims — reject if it does.",
  "boltffi design: consumer API inventory of the agent crates"
);
if (!inventory.approved) { log("✘ Inventory not approved — halting"); return { ok: false, phase: "Inventory", inventory }; }

phase("Design");
const design = await build(
  "Design",
  "You are the DESIGN agent (" + MODEL + ", xhigh). " + COMMON + "\n\n" +
    "SUPERSESSION NOTICE — the R2 requirement in the preamble above is RETIRED. The owner adopted a corrected statement of intent on 2026-08-26: read " + OUT + "/owner-review-2026-08-26.md IN FULL before anything else; it is authoritative for this design. The corrected R2: integration must not introduce a separately maintained FFI facade, duplicate record hierarchy, IDL, or required Swift wrapper; existing canonical crates may receive inline BoltFFI annotations and minimal concrete API changes needed to project their ordinary consumer contracts safely (owned returns, concrete collection inputs, interior synchronization, async pull methods, Rust-owned runtime integration) — every such change must remain a legitimate Rust API rather than a binding-only command/envelope layer.\n\n" +
    "Task: REVISE " + OUT + "/design.md in place (the prior revision is on disk and its documentation research is sound material) so it addresses EVERY section of the owner review explicitly:\n" +
    "1. The binding boundary is the concrete Tokio actor facade (TokioAgentHandle, TokioAgentRun, owned event/outcome/snapshot/control values, AgentEventSink) — NOT the borrowed pi-agent-core Agent seams; the ordinary-consumer export list and the keep-unannotated list from the review, verbatim as scope.\n" +
    "2. The streaming rule: no EventSubscription for authoritative AgentEvent/AssistantEvent delivery (re-verify the ring-buffer drop contract against the streaming.md snapshot and cite it); the exported boundary is the lossless async pull method; while-let consumption in Swift.\n" +
    "3. The canonical Rust API changes chapter — the minimal concrete changes the corrected R2 permits, each specified as a legitimate Rust API with rationale and file:line of the current shape: the TokioAgentRun reshape (interior synchronization, &self next_event/outcome, watch-style completion, cancel, cancel_and_outcome, EOF validation returning MissingRunFinished/SnapshotInvariant/Closed, raw receiver access kept in an unannotated Rust-only impl), the sink-only run fix (optional observational sender preferred), the AgentEventSink async-trait export preserving acknowledgement-barrier semantics, the Tokio runtime owner/factory in pi-agent-runtime-tokio, and the AgentEventEnvelope decision (present both options from the review with its recommendation and why it must be a canonical runtime change).\n" +
    "4. Cancellation semantics exactly as the review describes (Swift task cancellation vs run cancellation; cancellation-safe recv; the stall hazard when Swift never resumes).\n" +
    "5. Updated Swift consumer shapes matching the review's four sketches.\n" +
    "6. The acceptance-test plan: all 12 required tests from the review, each mapped to where and how it will be implemented.\n" +
    "7. Keep the per-page capability summary, mapping table for the in-scope surface, gaps, and phased implementation plan — re-scoped to the ordinary-consumer boundary; the phased plan's first milestone is the canonical Rust changes, then annotations, then the Swift acceptance tests.\n" +
    "CITATION RULES UNCHANGED: every BoltFFI claim ends with [URL | snapshot-path]; where the owner review asserts a BoltFFI behavior, re-verify it against the snapshot and cite (if the snapshot does not support a review claim, flag the discrepancy prominently rather than silently adopting either side); claims about our code carry file:line; UNRESOLVED only for questions the documentation genuinely does not answer.",
  "You are the DESIGN REVIEWER (" + MODEL + ", xhigh), a fresh session — the citation and conformance auditor. " + COMMON + "\n\n" +
    "SUPERSESSION NOTICE — the preamble R2 is retired; " + OUT + "/owner-review-2026-08-26.md is the authoritative intent. Read it in full first.\n\n" +
    "Adversarially verify design.md:\n" +
    "1. OWNER-REVIEW CONFORMANCE: walk the review section by section (corrected R2, streaming rule, actor-facade boundary, TokioAgentRun reshape, cancellation, sink semantics, sink-only fix, envelope decision, runtime ownership, consumer-scope lists, 12 acceptance tests) and confirm each is addressed as specified; a silently dropped or watered-down section is a rejection.\n" +
    "2. CITATIONS: every BoltFFI claim carries [URL | snapshot-path] and the cited text supports it — read the cited sections; for at least 10 claims across async, streaming, errors, custom-types, and functions ALSO re-fetch the live URL. Review claims about BoltFFI must have been re-verified, not just repeated.\n" +
    "3. CANONICAL-API DISCIPLINE: every proposed Rust change is a legitimate ordinary Rust API improvement (would make sense with no FFI present), not a binding-only envelope; the borrowed Agent seams, generics, provider traits, and Local family remain unannotated.\n" +
    "4. Our-code claims verified at file:line; Swift sketches use only documented generated features; UNRESOLVED items genuinely unanswered.\n" +
    "Reject with file/section-specific feedback; it is the author's only context next round.",
  "boltffi design: revised under the adopted owner review (async-pull boundary, canonical Rust API changes)"
);

phase("Closeout");
const ok = design.approved;
log((ok ? "✔" : "✘") + " boltffi design " + (ok ? "complete" : "NOT approved") + " — corpus " + corpus.rounds + "r, inventory " + inventory.rounds + "r, design " + design.rounds + "r");
return { ok, corpus, inventory, design, out: OUT };
