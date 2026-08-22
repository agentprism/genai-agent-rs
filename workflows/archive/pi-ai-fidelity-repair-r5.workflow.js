export const meta = {
  name: "pi-ai-fidelity-repair-r5",
  description:
    "R5: abort-path fidelity (pre-aborted ordering/message, mid-stream block finishing, no wire request when pre-aborted) and routing unknown-key preservation. codex/gpt-5.6-sol (xhigh) implements; claude/opus[1m] (xhigh) reviews; gated, commit on approval.",
  phases: [
    { title: "Setup" },
    { title: "R5 · abort paths + routing keys" },
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
    key: "r5",
    title: "R5 · abort paths + routing keys",
    commitMsg: "ai: R5 — abort-path fidelity (ordering, messages, block finishing, no pre-aborted wire request) and routing unknown-key preservation",
    scope:
      "SCOPE — two defect families, both to pi behavior. The pi paths below were traced line-by-line; verify them yourself before changing code.\n\n" +
      "E1 · ABORT PATHS (ai/src/api/openai_sse.rs, openai_completions.rs, openai_responses.rs, openai_responses_shared.rs as needed, openai_codex_responses.rs):\n" +
      "(a) Pre-aborted signal, completions/responses. pi performs params build, onPayload, api-key resolution and client creation BEFORE the request; the OpenAI SDK then throws APIUserAbortError at send time without putting a request on the wire (~/pi/node_modules/openai/client.js:357), and pi's retryProviderRequest converts ANY error thrown while options.signal.aborted into createAbortError() (provider-retry.ts:69-71,113-115) — so the observable errorMessage is exactly 'Request aborted' (formatProviderError returns the bare message when status is undefined, error-body.ts:126-133; same for responses' formatOpenAIResponsesError, openai-responses.ts:88-90). Today Rust short-circuits at openai_completions.rs:492-498 / openai_responses.rs:305-311 BEFORE those steps with 'Request was aborted'. Contract: no early short-circuit — a missing api key (or any pre-request failure pi hits first) surfaces first; onPayload is invoked exactly as in pi; when the request point is reached with an aborted signal, NO HTTP request is sent (openai_sse.rs send_request currently races the send future against wait_for_abort in a select! — the SDK checks before fetch), and the resulting errorMessage is 'Request aborted' with stopReason aborted. The existing retry wrapper already maps an error-while-aborted to ProviderRetryError::Abort => 'Request aborted' (utils/provider_retry.rs:96-102, openai_completions.rs:823, openai_responses.rs:856); build on it.\n" +
      "(b) Mid-stream abort, completions. pi's SDK stream iterator exits silently on abort (openai core/streaming.js:74-75,121-122); pi then runs finishBlock for every open block (openai-completions.ts:642-644), emitting the text_end/thinking_end/toolcall_end events, and only THEN throws 'Request was aborted' (:645-646). Rust's next_chunk (openai_completions.rs:781-810) returns Err immediately on signal.cancelled() or on an aborted-flagged stream error, skipping finish_blocks (:575). Contract: identical event sequence — open blocks end, then the error event with 'Request was aborted'. Responses: pi's processResponsesStream simply returns on the silent exit and openai-responses.ts:171-172 throws 'Request was aborted' — verify the Rust shared-processor path yields the same sequence and message; fix if not.\n" +
      "(c) Codex. Messages are already pi's ('Request was aborted'), but openai_codex_responses.rs:416-418 checks abort BEFORE api-key/accountId resolution, whereas pi resolves apiKey (:257-260) and accountId (:262), builds the body and runs onPayload (:267-271) first and checks abort at the transport request points (SSE :383-384, WS :322-323). Contract: same ordering — a missing key surfaces before abort; onPayload runs; the later checks (:500,:517,:588) remain the abort points.\n" +
      "TESTS: add an AbortSignal test double (types.rs defines the trait) and pin: pre-aborted → 'Request aborted' + stopReason aborted on both completions and responses; onPayload invoked and the hermetic server records NO request; pre-aborted + missing api key → the key error (completions, responses, codex); mid-stream abort (completions) → *_end events for open text/thinking/tool blocks precede the error event whose message is 'Request was aborted'; responses mid-stream abort message 'Request was aborted'. Existing tests asserting the old early-return behavior are defects — correct them.\n\n" +
      "E2 · ROUTING UNKNOWN KEYS (ai/src/types.rs): RoutingSortOptions (:1154), OpenRouterMaxPrice (:1166), PercentileThresholds (:1180), OpenRouterRouting (:1227), VercelGatewayRouting (:1264) drop unknown keys. pi loads these schema-free and forwards openRouterRouting verbatim as the `provider` request field (types.ts:596-599,718-722) — a custom key pi sends, Rust drops. Contract: unknown keys preserved on load and round-trip and reach the request body, exactly as the compat structs now do (the `#[serde(default, flatten)] extra: Map<String, Value>` idiom at types.rs:1325 and test known_api_compat_preserves_unknown_keys_as_inert_data at :2043); known-key wire form byte-identical; derives adjusted only as the change forces (Map<String, Value> is not Eq). Pin with a round-trip test per struct and one completions request-body test showing an unknown nested routing key on the wire.",
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
