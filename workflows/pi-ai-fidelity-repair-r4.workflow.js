export const meta = {
  name: "pi-ai-fidelity-repair-r4",
  description:
    "R4: resolve the residue found by the post-repair re-audit (audit doc section D). codex/gpt-5.6-sol (xhigh) implements; claude/opus[1m] (xhigh) reviews; gated, commit on approval.",
  phases: [
    { title: "Setup" },
    { title: "R4 · re-audit residue" },
  ],
};

const REPO = "/Users/vikashloomba/genai-agent";
const PI = "/Users/vikashloomba/pi/packages/ai";
const PIN = "496185f6e4267b979e3663c45f7eb70b0c6a97b4";
const AUDIT = REPO + "/docs/porting-pi-ai-and-agent-core-docs/openai-family-port-independent-audit-2026-08-20.md";
const IMPLEMENTER = "codex/gpt-5.6-sol";
const REVIEWER = "claude/opus[1m]";
const COMMITTER = "claude/haiku";
const MAX_ROUNDS = 4;

const COMMON =
  "GOVERNING RULES (identical for every phase):\n" +
  "- The reference is pi-ai at " + PI + " (repo pinned at commit " + PIN + "). Faithful means a consumer " +
  "of the Rust `ai` crate observes no feature or behavior difference from pi-ai beyond language " +
  "semantics: same request bytes and field-presence semantics, same event sequences and ordering, same " +
  "error strings where observable, same usage/cost math, same defaults, fallbacks, and retry behavior. " +
  "pi behavior is the default; never introduce a divergence — if pi does it observably, we do it.\n" +
  "- THE DEFECT LIST is the independent audit at " + AUDIT + ". Read it in full first. Your phase scope " +
  "names the audit items you must resolve. For each item: verify the cited pi lines yourself (the audit " +
  "is evidence, pi is the authority), fix to pi behavior, and add a regression test pinning the pi " +
  "behavior with a pi file:line citation in the test doc comment. Do not fix by weakening a test.\n" +
  "- Binding seam rulings: " + REPO + "/docs/porting-pi-ai-and-agent-core-docs/v2/preserved-architectural-seams-pi-ai-v2.mdx " +
  "and provider-api-implementations.mdx in the same tree (transport truths; audit doc section B has " +
  "verified openai-oxide 0.16 capability evidence).\n" +
  "- Work ONLY inside " + REPO + "/ai (plus ai/Cargo.toml and workspace Cargo.lock if a cached " +
  "dependency is needed). NEVER modify anything under " + PI + " or " + REPO + "/genai. Never run cargo " +
  "fmt on the genai/ subtree.\n" +
  "- Minimal, surgical diffs: fix the listed defects and their tests; no drive-by refactors, no style " +
  "passes, no renames beyond what a fix requires.\n" +
  "- Comments: only constraints the code cannot show, plus pi file.ts:line provenance anchors. No narration.\n" +
  "- Tests hermetic only (no network, no live keys).\n" +
  "- Gates you must run and make pass: cargo fmt -p agentprism-ai; cargo clippy -p agentprism-ai " +
  "--all-targets -- -D warnings; cargo test -p agentprism-ai; cargo build --workspace; git diff --check. " +
  "Use --offline if the sandbox denies network. If a command is sandbox-denied, land the code, report " +
  "the exact denied command, let the reviewer run it — never claim a gate passed that you did not see pass.\n" +
  "- Do NOT git commit; the workflow commits after review approval.\n";

const PHASES = [
  {
    key: "r4",
    title: "R4 · re-audit residue",
    commitMsg: "ai: R4 — re-audit residue: mid-stream error chunks, compat key preservation, error-body numbers, phase signature, SSE edge, qwen pin",
    scope:
      "SCOPE — resolve section D of the audit doc (post-repair re-audit residue), all to pi behavior:\n" +
      "- D1 (completions; consumer-observable): a streamed chunk carrying a truthy top-level `error` field must surface exactly as pi does. pi's SDK (node_modules/openai/core/streaming.js:49-50 under ~/pi) throws APIError(undefined, data.error, undefined, response.headers) for ANY such chunk; openai-completions.ts:664-683 then sets errorMessage from it (with the OpenRouter error.metadata.raw appendix) and stopReason=error. Today openai_sse.rs/openai_completions.rs ignore the field and end with 'Stream ended without finish_reason' (or a spurious success when supportsFinishReason=false). Mirror the same SDK behavior on the responses path (a data object with top-level `error` — distinct from the handled type:'error' event). Regression tests for both, incl. the metadata.raw appendix.\n" +
      "- D2 (types): known-api compat structs (OpenAICompletionsCompat etc., types.rs ~1273-1379) must preserve unknown keys on load and round-trip exactly as pi's schema-free JSON.parse does (types.ts:822-850) — the same inert-data parity the new Custom(Value) path already has. Keep the wire form of known keys byte-identical.\n" +
      "- D3 (utils/error_body.rs safe_json_stringify + openai_sse.rs error-body stringification): whole-number floats inside stringified error bodies serialize like JSON.stringify (1, not 1.0), consistent with the R1 wire-number rule.\n" +
      "- D4 (openai_responses_shared.rs ~1170-1179 deserialize_known_phase + encode of textSignature): pi includes ANY truthy phase string in the text signature (openai-responses-shared.ts:49-53,700) — an unknown phase must be preserved, not collapsed to None. The existing test unknown_message_content_and_phase_are_tolerated pins the divergent branch; correct it to pi's behavior (unknown content-part → '' stays).\n" +
      "- D5 (openai_sse.rs SseDecoder ~415-418): an SSE message with an `event:` line and no `data:` lines — pi's SDK dispatches {event, data:''} and JSON.parse('') throws, ending the stream with an error; match that outcome (in-band terminal error with pi's observable message path), not a silent skip. Pin it.\n" +
      "- D6 (openai_completions.rs qwen branch): add the missing regression test for qwen + explicit-null thinkingLevelMap entry → reasoning_effort:'<level>' (openai-completions.ts:842), so a nullish→defined mutation is caught.\n" +
      "NOT in scope (documented ε in section D, do not change): X-Stainless-* JS-runtime identity headers; JS string-concat/float semantics for off-spec usage token values; the compat-variant type guard.",
  },
];

function implPrompt(ph, feedback, attempt) {
  return (
    "You are the IMPLEMENTER (" + IMPLEMENTER + ", xhigh reasoning) working in " + REPO + ".\n\n" +
    COMMON + "\n" + ph.scope + "\n\n" +
    (feedback
      ? "A REVIEWER REJECTED your previous attempt (round " + attempt + "). Your prior edits are still " +
        "on disk; fix every point below without regressing what was already correct:\n" + feedback + "\n\n"
      : "") +
    "Return a concise plaintext report: audit items resolved (each with the regression test that pins " +
    "it), files changed, gate commands run with results, any sandbox-denied commands, and any pi " +
    "behavior you could not restore faithfully (do NOT silently deviate — report it)."
  );
}

function reviewPrompt(ph, implReport) {
  return (
    "You are the REVIEWER (" + REVIEWER + ", xhigh effort) in " + REPO + ".\n" +
    "STRICT RULE: do NOT modify, create, stage, or commit any file. Read and run verification commands " +
    "only.\n\n" +
    "The phase under review:\n" + ph.scope + "\n\n" + COMMON + "\n" +
    "The implementer reported:\n" + (implReport == null ? "(no report)" : String(implReport)) + "\n\n" +
    "Judge the ACTUAL working-tree changes (git status + git diff + reading the files) against pi at " +
    PI + " and the audit at " + AUDIT + ":\n" +
    "1. Every audit item in this phase's scope is resolved to PI behavior (verify against the pi source " +
    "lines yourself, not the audit's paraphrase) and carries a regression test citing pi file:line. " +
    "List each item with your verification.\n" +
    "2. No regressions: request bytes still byte-identical to pi; seams intact (streams never return " +
    "Result; partial-free protocol; open unions; headers sentinel); no unrelated churn in the diff.\n" +
    "3. Independently run: cargo fmt -p agentprism-ai -- --check; cargo clippy -p agentprism-ai " +
    "--all-targets -- -D warnings; cargo test -p agentprism-ai; cargo build --workspace; git diff " +
    "--check. Confirm changes touch only ai/ (+ Cargo.toml/lock if justified).\n" +
    "Set ok=true ONLY if every in-scope item is genuinely restored to pi behavior with no blocking " +
    "defect. Otherwise ok=false with complete file-and-line-specific feedback — it is the implementer's " +
    "ONLY context next round."
  );
}

const REVIEW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "feedback", "summary", "gatesRun", "blocking"],
  properties: {
    ok: { type: "boolean" },
    feedback: { type: "string" },
    summary: { type: "string" },
    gatesRun: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["command", "result"],
        properties: {
          command: { type: "string" },
          result: { type: "string", enum: ["pass", "fail", "environment-blocked"] },
          note: { type: "string" },
        },
      },
    },
    blocking: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["file", "issue"],
        properties: { file: { type: "string" }, issue: { type: "string" } },
      },
    },
  },
};

const COMMIT_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["committed", "sha", "note"],
  properties: {
    committed: { type: "boolean" },
    sha: { type: "string" },
    note: { type: "string" },
  },
};

phase("Setup");
log("pi-ai fidelity repair · impl=" + IMPLEMENTER + "/xhigh · rev=" + REVIEWER + "/xhigh · ≤" + MAX_ROUNDS + " rounds/phase · pi pin " + PIN.slice(0, 9));

const results = [];
let halted = false;

for (const ph of PHASES) {
  if (halted) {
    results.push({ phase: ph.key, approved: false, rounds: 0, sha: null, summary: "skipped: earlier phase not approved" });
    continue;
  }
  phase(ph.title);
  const outcome = await gate(
    (feedback, attempt) =>
      agent(implPrompt(ph, feedback, attempt), {
        label: "impl:" + ph.key + ":r" + (attempt + 1),
        phase: ph.title,
        model: IMPLEMENTER,
        mode: "agent",
        cwd: REPO,
        configOptions: { reasoning_effort: "xhigh" },
      }),
    (result) =>
      agent(reviewPrompt(ph, result), {
        label: "review:" + ph.key,
        phase: ph.title,
        model: REVIEWER,
        mode: "bypassPermissions",
        cwd: REPO,
        configOptions: { effort: "xhigh" },
        schema: REVIEW_SCHEMA,
      }),
    { attempts: MAX_ROUNDS }
  );

  const verdict = outcome.verdict || {};
  if (!outcome.ok) {
    log("✘ " + ph.key + " NOT approved after " + outcome.attempts + " round(s) — halting subsequent phases.");
    results.push({ phase: ph.key, approved: false, rounds: outcome.attempts, sha: null, summary: verdict.summary || "", blocking: verdict.blocking || [] });
    halted = true;
    continue;
  }

  const commit = await agent(
    "In " + REPO + ", stage and commit ALL current working-tree changes as ONE commit:\n" +
      "  git add -A\n  git commit -m '" + ph.commitMsg.replace(/'/g, "") + "'\n" +
      "Then read back the SHA with git rev-parse HEAD. If git status shows nothing to commit, make no " +
      "commit. Do NOT modify any file, amend history, push, or touch other branches.",
    { label: "commit:" + ph.key, phase: ph.title, model: COMMITTER, mode: "bypassPermissions", cwd: REPO, schema: COMMIT_SCHEMA }
  );
  const c = commit || { committed: false, sha: "", note: "commit agent returned null" };
  const shaOk = typeof c.sha === "string" && /^[0-9a-f]{7,40}$/i.test(c.sha.trim());
  const sha = c.committed === true && shaOk ? c.sha.trim() : null;
  log("✔ " + ph.key + " approved after " + outcome.attempts + " round(s)" + (sha ? " · committed " + sha.slice(0, 9) : " · ⚠ no commit: " + c.note));
  results.push({ phase: ph.key, approved: true, rounds: outcome.attempts, sha: sha, summary: verdict.summary || "" });
}

return { pin: PIN, audit: AUDIT, results: results };
