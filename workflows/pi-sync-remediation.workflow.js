export const meta = {
  name: "pi-sync-remediation",
  description:
    "Remediate owner-approved divergences from a pi-sync report: implement pinned pi's current behavior for each approved item, update tests/fixtures/manifest, all gates green. Owner-gated input: only items listed in args.items are touched. codex/gpt-5.6-sol xhigh only.",
  model: "codex/gpt-5.6-sol",
  phases: [{ title: "Preflight" }, { title: "Remediate" }, { title: "Closeout" }],
};

const A = typeof args === "string" ? JSON.parse(args) : args || {};
const REPO = A.repo || "/home/vikash/genai-agent/genai-agent-rs";
const BRANCH = A.branch || "main";
const REPORT = A.report || "docs/pi-sync/report-undated.md";
const ITEMS = Array.isArray(A.items) ? A.items : [];
const MAX_ROUNDS = A.maxRounds || 6;
const MODEL = "codex/gpt-5.6-sol";
const XHIGH = { reasoning_effort: "xhigh" };

const COMMON =
  "Repository: " + REPO + " (branch " + BRANCH + "). Governing documents: docs/porting-pi-ai-and-agent-core-docs/goal.md (Pin tracking section), the architecture parts 1 and 2, parity/manifest.toml (upstream_commit is the current pin; the pinned worktree is /home/vikash/pi-pin-<first-9-of-pin>). The owner divergence report for this cycle is " + REPORT + ".\n\n" +
  "REMEDIATION IS OWNER-GATED: you may remediate ONLY the approved items given below. pi source at the current pin is the behavior authority; the architecture documents are the authority for shape; divergences outside §10.11 and owner rulings are defects. Tests are hermetic. Gates: cargo fmt --all -- --check; cargo clippy --workspace --all-targets -- -D warnings; cargo build --workspace; cargo test --workspace; bash parity/check.sh; git diff --check. Never claim a gate you did not see pass. Do not git commit; the workflow commits after approval.";

const REVIEW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "feedback", "summary", "blocking"],
  properties: {
    ok: { type: "boolean", description: "true only if every approved item matches pinned pi behavior, nothing else changed behaviorally, and all gates are green" },
    feedback: { type: "string", description: "Specific feedback for the next round; empty when ok" },
    summary: { type: "string", description: "Up to five lines: what was verified and how" },
    blocking: {
      type: "array",
      description: "Blocking defects",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["where", "issue"],
        properties: { where: { type: "string", description: "file:line" }, issue: { type: "string", description: "What is wrong" } },
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

if (!ITEMS.length) { log("✘ no approved items supplied"); return { ok: false, reason: "no items" }; }

phase("Preflight");
const pf = await agent(
  "Preflight only; change nothing. In " + REPO + ": report `git branch --show-current`, whether `git status --porcelain` is empty, and the parity/manifest.toml upstream_commit. ok=true only if the branch is '" + BRANCH + "' and the tree is clean.",
  {
    label: "preflight", phase: "Preflight", model: MODEL, mode: "read-only", cwd: REPO, configOptions: { reasoning_effort: "low" },
    schema: {
      type: "object", additionalProperties: false, required: ["ok", "branch", "clean", "pin", "note"],
      properties: {
        ok: { type: "boolean", description: "branch matches and tree clean" },
        branch: { type: "string", description: "current branch" },
        clean: { type: "boolean", description: "porcelain empty" },
        pin: { type: "string", description: "manifest upstream_commit" },
        note: { type: "string", description: "anything off" },
      },
    },
  }
);
if (!pf || !pf.ok) { log("✘ preflight failed: " + JSON.stringify(pf)); return { ok: false, preflight: pf }; }

phase("Remediate");
const results = [];
for (const item of ITEMS) {
  const outcome = await gate(
    (feedback, attempt) =>
      agent(
        "You are the REMEDIATION agent (" + MODEL + ", xhigh). " + COMMON + "\n\nAPPROVED ITEM " + item.id + " — " + item.title + "\n" + item.instructions +
          "\n\nRead the report's row for this item and the cited pi source at the current pin in full before editing. Report in plaintext: files changed, the pi lines matched, tests updated/added, manifest mappings touched, each gate command and its observed result." +
          (feedback ? "\n\nA REVIEWER REJECTED the previous attempt (round " + attempt + "). Files are on disk; fix every point:\n" + feedback : ""),
        { label: "impl:" + item.id + ":r" + (attempt + 1), phase: "Remediate", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: XHIGH, retries: 1 }
      ),
    (result) =>
      agent(
        "You are the REMEDIATION REVIEWER (" + MODEL + ", xhigh), a fresh session. " + COMMON + "\n\nAPPROVED ITEM " + item.id + " — " + item.title + "\n" + item.instructions + "\n\nThe implementer reported:\n" + (result == null ? "(no report)" : String(result)) +
          "\n\nVerify adversarially: (1) read the pi source at the current pin and confirm the Rust change reproduces pi's CURRENT behavior exactly, including edge cases the report row names; (2) confirm nothing outside this item changed behaviorally (git diff); (3) tests assert the new behavior and cite pi; affected wire fixtures/goldens re-verified; needs-reverification mappings resolved truthfully; (4) run every gate yourself. ok=false with file:line specifics otherwise.",
        { label: "review:" + item.id, phase: "Remediate", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: XHIGH, schema: REVIEW_SCHEMA, retries: 1 }
      ),
    { attempts: MAX_ROUNDS }
  );
  if (!outcome.ok) { log("✘ " + item.id + " NOT approved after " + outcome.attempts + " round(s) — halting"); results.push({ id: item.id, approved: false, rounds: outcome.attempts }); return { ok: false, results, verdict: outcome.verdict || {} }; }
  const commit = await agent(
    "In " + REPO + ": git add crates providers bindings parity docs/pi-sync Cargo.toml Cargo.lock && git commit -m 'pi-sync remediation " + item.id + ": " + String(item.title).replace(/'/g, "") + "'. Report git rev-parse HEAD. Do not modify files, amend, push, or touch other branches.",
    { label: "commit:" + item.id, phase: "Remediate", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: { reasoning_effort: "low" }, schema: COMMIT_SCHEMA }
  );
  const sha = commit && commit.committed === true ? String(commit.sha).trim() : null;
  log("✔ " + item.id + " approved after " + outcome.attempts + " round(s)" + (sha ? " · committed " + sha.slice(0, 9) : " · ⚠ no commit"));
  results.push({ id: item.id, approved: true, rounds: outcome.attempts, sha });
}

phase("Closeout");
log("✔ pi-sync remediation complete: " + results.map((r) => r.id).join(", "));
return { ok: true, results };
