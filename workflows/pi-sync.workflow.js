export const meta = {
  name: "pi-sync",
  description:
    "Move the tracked pi pin to latest main, regenerate the manifest inventory, and produce an owner divergence report classifying every behavioral change in pi against the port. Report-only: remediation is owner-gated and happens in a separate approved run. codex/gpt-5.6-sol xhigh only.",
  model: "codex/gpt-5.6-sol",
  phases: [{ title: "Preflight" }, { title: "Sync" }, { title: "Closeout" }],
};

const A = typeof args === "string" ? JSON.parse(args) : args || {};
const REPO = A.repo || "/home/vikash/genai-agent/genai-agent-rs";
const BRANCH = A.branch || "main";
const PI_GIT = A.piGit || "/home/vikash/pi";
const DATE = A.date || "undated";
const MAX_ROUNDS = A.maxRounds || 6;
const INITIAL_FEEDBACK = typeof A.initialFeedback === "string" && A.initialFeedback.trim() ? A.initialFeedback : null;
const MODEL = "codex/gpt-5.6-sol";
const XHIGH = { reasoning_effort: "xhigh" };
const OUT = "docs/pi-sync";

const COMMON =
  "Repository: " + REPO + " (branch " + BRANCH + "). Governing documents: docs/porting-pi-ai-and-agent-core-docs/goal.md (note the Pin tracking section: the pin is a tracked cursor; remediation is OWNER-GATED), the architecture parts 1 and 2, and parity/manifest.toml (upstream_commit is the current pin). The local pi clone is at " + PI_GIT + "; pinned worktrees live at /home/vikash/pi-pin-<short-sha>.\n\n" +
  "THIS RUN CHANGES NO CRATE CODE. It moves the pin bookkeeping and produces the owner divergence report. You may modify ONLY: parity/manifest.toml (upstream_commit, inventory additions for new upstream test files as planned with milestone \"SYNC-" + DATE + "\", and per-mapping needs-reverification notes), parity/upstream-tests.txt, the PIN default and piRoot default in workflows/architecture-v2-milestones.workflow.js, and " + OUT + "/. Never modify crates/, providers/ code, bindings/, or the architecture documents. Report only what you actually observed.";

const REVIEW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "feedback", "summary", "blocking"],
  properties: {
    ok: { type: "boolean", description: "true only if the sync bookkeeping is correct and the report is complete and truthful" },
    feedback: { type: "string", description: "Specific feedback for the next round; empty when ok" },
    summary: { type: "string", description: "Up to five lines: what was verified and how" },
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

phase("Preflight");
const pf = await agent(
  "Preflight only; change nothing. In " + REPO + ": report `git branch --show-current` and whether `git status --porcelain` is empty; read parity/manifest.toml upstream_commit (the current pin). In " + PI_GIT + ": run `git fetch origin main` and report `git rev-parse origin/main`. ok=true only if the branch is '" + BRANCH + "', the tree is clean, and the fetch succeeded. Also report whether origin/main equals the current pin (nothing to sync).",
  {
    label: "preflight", phase: "Preflight", model: MODEL, mode: "agent", cwd: REPO, configOptions: { reasoning_effort: "low" },
    schema: {
      type: "object", additionalProperties: false, required: ["ok", "branch", "clean", "currentPin", "latest", "upToDate", "note"],
      properties: {
        ok: { type: "boolean", description: "branch matches, tree clean, fetch succeeded" },
        branch: { type: "string", description: "current branch" },
        clean: { type: "boolean", description: "porcelain empty" },
        currentPin: { type: "string", description: "manifest upstream_commit" },
        latest: { type: "string", description: "pi origin/main sha" },
        upToDate: { type: "boolean", description: "latest equals current pin" },
        note: { type: "string", description: "anything off" },
      },
    },
  }
);
if (!pf || !pf.ok) { log("✘ preflight failed: " + JSON.stringify(pf)); return { ok: false, preflight: pf }; }
if (pf.upToDate) { log("✔ pin already at pi origin/main (" + String(pf.latest).slice(0, 9) + ") — nothing to sync"); return { ok: true, upToDate: true, pin: pf.latest }; }

phase("Sync");
const outcome = await gate(
  (feedback, attempt) =>
    agent(
      "You are the SYNC agent (" + MODEL + ", xhigh). " + COMMON + "\n\n" +
        "The pin moves from " + pf.currentPin + " to " + pf.latest + ". Do, in order:\n" +
        "1. Create the new worktree if absent: in " + PI_GIT + ", `git worktree add --detach /home/vikash/pi-pin-<first-9-of-new-sha> " + pf.latest + "`.\n" +
        "2. Read `git log --stat " + pf.currentPin + ".." + pf.latest + " -- packages/ai packages/agent` and the full diff for every changed source/test file under those packages.\n" +
        "3. Classify every changed file: BEHAVIORAL (observable contract/behavior a consumer or our conformance tests could see), MECHANICAL (types-only, comments, versions, formatting), TEST (upstream test added/changed/removed), or OUT-OF-SCOPE (paths the port does not cover, e.g. unported subsystems — say which ruling/scope covers them).\n" +
        "4. For each BEHAVIORAL change, read our corresponding Rust code and determine the port's ACTUAL current state: ALREADY-CONFORMS (port matches new pi behavior; say why), DIVERGES (port implements the old behavior; describe old vs new precisely with file:line on both sides), or UNCLEAR (needs a decision; explain).\n" +
        "5. Bookkeeping: set parity/manifest.toml upstream_commit to " + pf.latest + "; regenerate parity/upstream-tests.txt from the new worktree; add [[mapping]] entries for NEW upstream test files as status planned, milestone \"SYNC-" + DATE + "\"; for existing mappings whose upstream file CHANGED, do not change their status — list them in the report as needs-reverification. Update the PIN and piRoot defaults in workflows/architecture-v2-milestones.workflow.js to the new sha/worktree. Run `bash parity/check.sh` against the new worktree and make it pass.\n" +
        "6. Write " + OUT + "/report-" + DATE + ".md — the OWNER DIVERGENCE REPORT: header (old pin, new pin, commit count, date); a DECISIONS-NEEDED table first (every DIVERGES and UNCLEAR item: upstream change, our behavior, impact, your recommendation, effort estimate); then ALREADY-CONFORMS; then needs-reverification mappings; then MECHANICAL/TEST/OUT-OF-SCOPE inventories. The report must be complete — every changed file appears exactly once. NO REMEDIATION: do not change crate code; the owner decides what gets remediated.\n" +
        "Report in plaintext what you did, the classification counts, and the DECISIONS-NEEDED count." +
        ((feedback = feedback || (attempt === 0 ? INITIAL_FEEDBACK : null)) ? "\n\nA REVIEWER REJECTED the previous attempt (round " + attempt + "). Files are on disk; fix every point:\n" + feedback : ""),
      { label: "sync:r" + (attempt + 1), phase: "Sync", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: XHIGH, retries: 1 }
    ),
  (result) =>
    agent(
      "You are the SYNC REVIEWER (" + MODEL + ", xhigh), a fresh session. " + COMMON + "\n\nThe sync agent reported:\n" + (result == null ? "(no report)" : String(result)) + "\n\n" +
        "Verify adversarially: (1) walk `git log " + pf.currentPin + ".." + pf.latest + " -- packages/ai packages/agent` yourself and confirm every changed file appears exactly once in the report with a defensible classification — spot-read at least 8 diffs including every file classified BEHAVIORAL; (2) for each DIVERGES/ALREADY-CONFORMS verdict, read our Rust code at the cited lines and confirm the verdict; (3) bookkeeping: manifest upstream_commit, upstream-tests.txt, new-test mappings, workflow PIN defaults, parity/check.sh passing, new worktree present; (4) NO crate code changed (git status shows only the allowed paths); (5) the report contains no remediation. ok=false with specifics otherwise.",
      { label: "review:sync", phase: "Sync", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: XHIGH, schema: REVIEW_SCHEMA, retries: 1 }
    ),
  { attempts: MAX_ROUNDS }
);
if (!outcome.ok) { log("✘ sync NOT approved after " + outcome.attempts + " round(s)"); return { ok: false, phase: "Sync", verdict: outcome.verdict || {} }; }

phase("Closeout");
const commit = await agent(
  "In " + REPO + ": git add parity workflows/architecture-v2-milestones.workflow.js " + OUT + " && git commit -m 'pi-sync " + DATE + ": pin " + String(pf.currentPin).slice(0, 9) + " -> " + String(pf.latest).slice(0, 9) + "; owner divergence report'. Report git rev-parse HEAD. Do not modify files, amend, push, or touch other branches.",
  { label: "commit:sync", phase: "Closeout", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: { reasoning_effort: "low" }, schema: COMMIT_SCHEMA }
);
const sha = commit && commit.committed === true ? String(commit.sha).trim() : null;
log("✔ pi-sync complete after " + outcome.attempts + " round(s)" + (sha ? " · committed " + sha.slice(0, 9) : " · ⚠ no commit: " + (commit ? commit.note : "null")));
return { ok: true, oldPin: pf.currentPin, newPin: pf.latest, rounds: outcome.attempts, sha, report: OUT + "/report-" + DATE + ".md" };
