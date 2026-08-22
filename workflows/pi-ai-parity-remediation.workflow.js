export const meta = {
  name: "pi-ai-parity-remediation",
  description:
    "Remediate the agentprism-ai crate to docs/porting-pi-ai-and-agent-core-docs/goal.md using the work packages produced by pi-ai-parity-audit: per package, a codex/gpt-5.6-sol xhigh implementer and an independent codex/gpt-5.6-sol xhigh reviewer (fresh session) gate up to four rounds, then a commit; a closeout pass re-runs the README litmus and records status. No Claude agents.",
  model: "codex/gpt-5.6-sol",
  phases: [{ title: "Preflight" }, { title: "Remediate" }, { title: "Closeout" }],
};

const A = typeof args === "string" ? JSON.parse(args) : args || {};
const REPO = A.repo || "/home/vikash/genai-agent/genai-agent-rs";
const PI = A.pi || "/home/vikash/pi-pin-c49906ec7/packages/ai";
const PIN = A.pin || "c49906ec77788625aacbdc53ebca6fbe65bd20f5";
const BRANCH = A.branch || "main";
const REPORT = A.reportPath || "";
const PACKAGES = Array.isArray(A.packages) ? A.packages : [];
const GOAL = REPO + "/docs/porting-pi-ai-and-agent-core-docs/goal.md";
const MODEL = "codex/gpt-5.6-sol";
const XHIGH = { reasoning_effort: "xhigh" };
const MAX_ROUNDS = A.maxRounds || 4;

if (!PACKAGES.length) {
  log("✘ no packages supplied in args; run pi-ai-parity-audit first and pass its `packages` (and `reportPath`)");
  return { ok: false, reason: "no packages" };
}

const COMMON =
  "GOAL (the owner's, and the only standard that matters): read " + GOAL + " in full before anything else. " +
  "In short — the agentprism-ai crate at " + REPO + "/ai must BE pi-ai: every feature and every observable " +
  "behavior of pi-ai exists in the crate, written as idiomatic Rust (ownership, Result, traits, async, real " +
  "types), never impersonating the JS runtime or JS SDKs (no spawning node, no fabricated runtime or SDK " +
  "versions; truthful identity on the wire; pi's own intentional headers such as `User-Agent: pi (...)` stay). " +
  "Litmus: any pi-ai README example or test must be recreatable against the crate with the same observable " +
  "results, without a workaround and without reading the crate internals. When idiom and parity seem to " +
  "conflict, parity wins and idiom adapts; a feature is never dropped for efficiency or convenience. Internals " +
  "are free; silent deviation is not. Anything genuinely unpreservable is a four-column row — Delta | Technical " +
  "delta | pi counterpart (file:line) | Why it cannot be preserved — for the owner; a row Rust can achieve is a " +
  "defect.\n\n" +
  "AUTHORITY: pi-ai source at " + PI + " (repo pinned at " + PIN + ") is the only authority. Read the pi files " +
  "for your package in full before changing anything. The audit report " + (REPORT || "(path in args)") +
  " and the package text are a map, not authority; the docs under " + REPO + "/docs are background. Where any " +
  "of them disagrees with pi, pi wins — say so in your report.\n\n" +
  "OWNER RULINGS (fixed): not ported — api/lazy.ts and *.lazy.ts, azure-openai-responses, " +
  "mistral-conversations, pi-messages, image generation (openrouter-images, images*.ts, image-models*, " +
  "images-api-registry, providers/images/*, providers/openrouter-images.ts), the agent package's proxy protocol, " +
  "Windows. SDKs: openai-completions/openai-responses → openai-oxide; anthropic-messages → adk-anthropic; " +
  "google-generative-ai/google-vertex → adk-gemini; bedrock-converse-stream → aws-sdk-bedrockruntime + " +
  "aws-config; other crates are free where faithfulness decides. Ruled 2026-08-21: an SDK may be extended or " +
  "narrowly forked (vendored) where pi's hook, header, or response-surface needs are not met by its public API — " +
  "the bar stays observable equivalence with what pi does with its SDK. Credential storage is host-pluggable and " +
  "never shells out.\n\n" +
  "CONSTRAINTS: work only inside " + REPO + "/ai (plus ai/Cargo.toml, the workspace Cargo.lock, and the docs " +
  "under " + REPO + "/docs/porting-pi-ai-and-agent-core-docs when a package says so). Never modify " + PI + ", " +
  REPO + "/genai, " + REPO + "/agent, or " + REPO + "/ffi. Layout mirrors pi file-for-file. Comments only for " +
  "constraints the code cannot show, plus pi file.ts:line provenance anchors. Tests are part of the work: port " +
  "pi's test cases for the package's subjects hermetically (no network, no live keys); where pi has no unit test " +
  "for a behavior, pin it with a test citing the pi line. Remove tests that pin behavior contradicting pi. " +
  "Gates: cargo fmt -p agentprism-ai; cargo clippy -p agentprism-ai --all-targets -- -D warnings; cargo test " +
  "-p agentprism-ai; cargo build --workspace; cargo build -p agentprism-ai --examples; git diff --check. Never " +
  "claim a gate you did not see pass. Do not git commit; the workflow commits after approval.\n";

function implPrompt(pkg, feedback, attempt) {
  return (
    "You are the IMPLEMENTER (" + MODEL + ", xhigh reasoning) working in " + REPO + ".\n\n" + COMMON + "\n" +
    "WORK PACKAGE " + pkg.id + " — " + pkg.title + "\n" +
    "Resolves audit findings: " + (pkg.findingIds || []).join(", ") + "\n" +
    "Files expected to change: " + (pkg.files || []).join(", ") + "\n" +
    "Scope:\n" + pkg.scope + "\n\n" +
    "Acceptance (the reviewer will check exactly this):\n" + pkg.acceptance + "\n\n" +
    (feedback
      ? "A REVIEWER REJECTED your previous attempt (round " + attempt + "). Your prior edits are still on disk; " +
        "address every point below without regressing what was already faithful:\n" + feedback + "\n\n"
      : "") +
    "If the working tree contains uncommitted work, it is material, not truth: audit it against pi, keep what " +
    "is faithful, fix the rest. Report, in plaintext: files added/changed; what pi does and what the crate now " +
    "does, with pi file:line anchors; the pi-test → Rust-test mapping (ported / pinned-from-source / " +
    "skipped-with-reason); each gate command and its observed result; anything the sandbox denied; any place " +
    "the package text, a doc, or existing code disagreed with pi and what you did; and the four-column " +
    "CANNOT-PRESERVE table — empty if nothing was left behind."
  );
}

function reviewPrompt(pkg, implReport) {
  return (
    "You are the REVIEWER (" + MODEL + ", xhigh reasoning) in " + REPO + ". You are a fresh, independent " +
    "session: you did not write this code. You do not modify, create, stage, or commit files; you read and run " +
    "verification commands.\n\n" + COMMON + "\n" +
    "WORK PACKAGE " + pkg.id + " — " + pkg.title + "\nScope:\n" + pkg.scope + "\n\nAcceptance:\n" + pkg.acceptance + "\n\n" +
    "The implementer reported:\n" + (implReport == null ? "(no report)" : String(implReport)) + "\n\n" +
    "Judge the actual working-tree changes (`git status`, `git diff`) against pi's source as an adversary " +
    "looking for anything a pi-ai consumer would notice:\n" +
    "1. Complete: every item in scope and every acceptance check is met; nothing deferred or stubbed.\n" +
    "2. Faithful: read the pi source for the scope and compare behavior, including failure paths and what is " +
    "persisted/sent. List what you verified and how.\n" +
    "3. Idiomatic and truthful: no runtime/SDK impersonation, no process spawning, no JS-isms for their own " +
    "sake; but every observable JS semantic pi exposes is kept.\n" +
    "4. Litmus: where the package touches the public surface, write (in your head or a scratch file you delete) " +
    "the README snippet against the crate and confirm it needs no workaround.\n" +
    "5. Tests: pi's tests for the scope are ported or accounted for; pins cite pi lines; everything hermetic; no " +
    "test pins a contradiction of pi.\n" +
    "6. Gates: run them yourself — cargo fmt -p agentprism-ai -- --check; cargo clippy -p agentprism-ai " +
    "--all-targets -- -D warnings; cargo test -p agentprism-ai; cargo build --workspace; cargo build -p " +
    "agentprism-ai --examples; git diff --check — and confirm only ai/ (+ Cargo.toml/lock and the named docs) " +
    "changed.\n" +
    "7. Rows: judge every CANNOT-PRESERVE row on its own; a row Rust can achieve is a rejection.\n" +
    "ok=true only if the package is complete and faithful with no blocking defect. Otherwise ok=false with " +
    "specific file-and-line feedback — it is the implementer's only context next round."
  );
}

const REVIEW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "feedback", "summary", "gatesRun", "blocking", "cannotPreserve"],
  properties: {
    ok: { type: "boolean", description: "true only if complete, faithful, idiomatic, gates green" },
    feedback: { type: "string", description: "Specific file-and-line feedback for the next round; empty when ok" },
    summary: { type: "string", description: "Five lines: what was verified and how" },
    gatesRun: {
      type: "array",
      description: "Every gate you ran and what you observed",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["command", "result"],
        properties: {
          command: { type: "string", description: "Exact command" },
          result: { type: "string", enum: ["pass", "fail", "environment-blocked"], description: "Observed result" },
          note: { type: "string", description: "Short note, e.g. test counts or the blocking error" },
        },
      },
    },
    blocking: {
      type: "array",
      description: "Blocking defects, each with file and issue",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["file", "issue"],
        properties: { file: { type: "string", description: "Rust file:line" }, issue: { type: "string", description: "What is wrong versus pi (cite pi file:line)" } },
      },
    },
    cannotPreserve: {
      type: "array",
      description: "Rows you accept as genuinely unpreservable, each judged on its own",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["delta", "technicalDelta", "piCounterpart", "why"],
        properties: {
          delta: { type: "string", description: "Delta" },
          technicalDelta: { type: "string", description: "Technical delta" },
          piCounterpart: { type: "string", description: "pi counterpart file:line" },
          why: { type: "string", description: "Why it cannot be preserved" },
        },
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
    note: { type: "string", description: "git show --stat summary line, or why nothing was committed" },
  },
};

const PREFLIGHT_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "branch", "head", "clean", "piHead", "note"],
  properties: {
    ok: { type: "boolean", description: "true only if branch matches, tree is clean, and the pi worktree is at the pin" },
    branch: { type: "string", description: "git branch --show-current in the repo" },
    head: { type: "string", description: "git rev-parse HEAD in the repo" },
    clean: { type: "boolean", description: "git status --porcelain is empty" },
    piHead: { type: "string", description: "git rev-parse HEAD in the pi worktree" },
    note: { type: "string", description: "Anything off" },
  },
};

phase("Preflight");
log("pi-ai parity remediation · " + PACKAGES.length + " packages · " + MODEL + "/xhigh impl+review · ≤" + MAX_ROUNDS + " rounds · pin " + PIN.slice(0, 9));
const preflight = await agent(
  "Preflight only; change nothing. In " + REPO + ": report `git branch --show-current`, `git rev-parse HEAD`, and " +
    "whether `git status --porcelain` is empty. In " + PI + ": report `git rev-parse HEAD`. ok=true only if the " +
    "branch is '" + BRANCH + "', the tree is clean, and the pi HEAD is " + PIN + ".",
  { label: "preflight", phase: "Preflight", model: MODEL, mode: "read-only", cwd: REPO, configOptions: { reasoning_effort: "low" }, schema: PREFLIGHT_SCHEMA }
);
if (!preflight || !preflight.ok) {
  log("✘ preflight failed: " + JSON.stringify(preflight));
  return { ok: false, reason: "preflight", preflight };
}

phase("Remediate");
const results = [];
let halted = false;
for (const pkg of PACKAGES) {
  if (halted) {
    results.push({ id: pkg.id, approved: false, rounds: 0, sha: null, summary: "skipped: earlier package not approved" });
    continue;
  }
  const outcome = await gate(
    (feedback, attempt) =>
      agent(implPrompt(pkg, feedback, attempt), {
        label: "impl:" + pkg.id + ":r" + (attempt + 1),
        phase: "Remediate",
        model: MODEL,
        mode: "agent-full-access",
        cwd: REPO,
        configOptions: XHIGH,
        retries: 1,
      }),
    (result) =>
      agent(reviewPrompt(pkg, result), {
        label: "review:" + pkg.id,
        phase: "Remediate",
        model: MODEL,
        mode: "agent-full-access",
        cwd: REPO,
        configOptions: XHIGH,
        schema: REVIEW_SCHEMA,
        retries: 1,
      }),
    { attempts: MAX_ROUNDS }
  );
  const verdict = outcome.verdict || {};
  if (!outcome.ok) {
    log("✘ " + pkg.id + " NOT approved after " + outcome.attempts + " round(s) — halting subsequent packages.");
    results.push({ id: pkg.id, approved: false, rounds: outcome.attempts, sha: null, summary: verdict.summary || "", blocking: verdict.blocking || [] });
    halted = true;
    continue;
  }
  const commit = await agent(
    "In " + REPO + ", commit the package's work as ONE commit scoped to the crate and named docs:\n" +
      "  git add ai Cargo.lock docs/porting-pi-ai-and-agent-core-docs\n" +
      "  git commit -m 'ai: " + pkg.id + " — " + String(pkg.title).replace(/'/g, "") + "'\n" +
      "Then read back the SHA with git rev-parse HEAD. If nothing is staged, make no commit. Do not modify any " +
      "file, amend history, push, or touch other branches.",
    { label: "commit:" + pkg.id, phase: "Remediate", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: { reasoning_effort: "low" }, schema: COMMIT_SCHEMA }
  );
  const c = commit || { committed: false, sha: "", note: "commit agent returned null" };
  const sha = c.committed === true && /^[0-9a-f]{7,40}$/i.test(String(c.sha).trim()) ? String(c.sha).trim() : null;
  const rows = Array.isArray(verdict.cannotPreserve) ? verdict.cannotPreserve.length : 0;
  log("✔ " + pkg.id + " approved after " + outcome.attempts + " round(s)" + (sha ? " · committed " + sha.slice(0, 9) : " · ⚠ no commit: " + c.note) + (rows ? " · cannot-preserve rows: " + rows : ""));
  results.push({ id: pkg.id, approved: true, rounds: outcome.attempts, sha, summary: verdict.summary || "", cannotPreserve: verdict.cannotPreserve || [] });
}

phase("Closeout");
const approved = results.filter((r) => r.approved);
const closeout = halted
  ? null
  : await agent(
      "You are the CLOSEOUT agent (" + MODEL + ", xhigh reasoning) in " + REPO + ".\n\n" + COMMON + "\n" +
        "All packages were approved and committed: " + JSON.stringify(approved.map((r) => ({ id: r.id, sha: r.sha }))) + "\n" +
        "Do three things. (1) Litmus: build `cargo build -p agentprism-ai --examples`; read ai/examples/quickstart.rs " +
        "against pi's README Quick Start (" + PI + "/../README.md) and make it a line-for-line recreation now that " +
        "the crate carries what pi carries (e.g. read `event.partial` directly; remove any builder workaround); if " +
        "DEEPSEEK_API_KEY is set in the environment, run it and confirm it behaves. (2) Record: update " +
        (REPORT || "the audit report under docs/porting-pi-ai-and-agent-core-docs") + " with a status column/section " +
        "marking each finding resolved (commit SHA) or carried as an owner-judged row, and set the reference pin in " +
        "ai/src/lib.rs's module doc to " + PIN + ". (3) Gates, then commit everything as ONE commit " +
        "'ai: parity remediation closeout — README litmus, audit status, pin " + PIN.slice(0, 9) + "' and report the SHA.",
      { label: "closeout", phase: "Closeout", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: XHIGH, schema: COMMIT_SCHEMA, retries: 1 }
    );

return {
  ok: !halted,
  pin: PIN,
  packages: results,
  closeout: closeout || null,
  cannotPreserve: approved.flatMap((r) => (r.cannotPreserve || []).map((row) => ({ package: r.id, ...row }))),
};
