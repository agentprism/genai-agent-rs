export const meta = {
  name: "pi-ai-full-port-v2",
  description:
    "Port the remainder of pi-ai into the agentprism-ai crate under the owner's standing directive: idiomatic Rust, all behavior and all features preserved, semantics where possible, anything unpreservable reported as a four-column row. Scopes name pi files only; pi source is the sole authority; docs are context. codex/gpt-5.6-sol (xhigh) implements; claude/opus[1m] (xhigh) reviews; gated, commit per approved phase.",
  phases: [
    { title: "Setup" },
    { title: "Q1 · utils, auth core, store/catalog primitives, observed deltas" },
    { title: "Q2 · models.ts, catalog data, faux, openai-family provider factories" },
    { title: "Q3 · anthropic-messages + its providers" },
    { title: "Q4 · google-shared, google-generative-ai, google-vertex + providers" },
    { title: "Q5 · bedrock-converse-stream + provider" },
    { title: "Q6 · OAuth flows" },
    { title: "Q7 · cloudflare, radius, all.ts, compat, legacy aliases, cli, final sweep" },
  ],
};

const REPO = "/Users/vikashloomba/genai-agent";
const PI = "/Users/vikashloomba/pi/packages/ai";
const PIN = "496185f6e4267b979e3663c45f7eb70b0c6a97b4";
const DOCS = REPO + "/docs/porting-pi-ai-and-agent-core-docs";
const AUDIT = DOCS + "/openai-family-port-independent-audit-2026-08-20.md";
const IMPLEMENTER = "codex/gpt-5.6-sol";
const REVIEWER = "claude/opus[1m]";
const COMMITTER = "claude/haiku";
const MAX_ROUNDS = 4;

const COMMON =
  "STANDING DIRECTIVE (the owner's, and the only standard that matters):\n" +
  "Port pi-ai into idiomatic Rust, preserving all behavior, preserving all features, and preserving semantics " +
  "where possible. If a behavior or feature genuinely cannot be preserved in Rust, do not silently deviate or " +
  "descope: implement the closest faithful behavior and report it as a row with exactly these four columns — " +
  "Delta | Technical delta | pi counterpart (file:line) | Why it cannot be preserved. The owner judges those rows. " +
  "The reviewer rejects any row that Rust can in fact achieve.\n" +
  "\n" +
  "AUTHORITY: pi-ai source at " + PI + " (repo pinned at " + PIN + ") is the authority for every behavior — " +
  "not this prompt, not the docs, not existing Rust code. Read the pi files you are porting in full before " +
  "porting them. Where anything in this prompt disagrees with pi, pi wins; say so in your report.\n" +
  "\n" +
  "OWNER RULINGS:\n" +
  "- Not ported: api/lazy.ts and *.lazy.ts (lazy module loading); azure-openai-responses; mistral-conversations; " +
  "pi-messages; image generation (openrouter-images, images*.ts, image-models*, images-api-registry, " +
  "providers/images/*, providers/openrouter-images.ts); the agent package's proxy protocol; Windows. Everything " +
  "else under " + PI + "/src is in scope.\n" +
  "- Dependencies are already decided where pi uses a provider SDK: openai-completions / openai-responses → " +
  "openai-oxide (already in the crate); anthropic-messages → adk-anthropic; google-generative-ai / google-vertex → " +
  "adk-gemini; bedrock-converse-stream → aws-sdk-bedrockruntime with aws-config. Use these; do not look for " +
  "alternatives. Where pi hand-rolls (the codex transport, SSE parsing pi does itself, the OAuth flows), port pi's " +
  "logic. The bar for an SDK-backed module is observable equivalence of pi's implementation: the Rust SDK may " +
  "differ internally from the TypeScript SDK pi uses — its own internals, helper semantics, framing — and that is " +
  "acceptable; what is not acceptable is our module doing something different with its SDK from what pi does with " +
  "its SDK, behaviorally or in practice (what is sent, how responses, errors, retries, and hooks are handled). Read " +
  "the chosen SDK's vendored source (~/.cargo/registry/src/) so you know what it actually does; how much of the " +
  "module's behavior the SDK carries versus our own code is your call against that bar. If something pi relies on " +
  "genuinely cannot be achieved, it is a four-column row. Other crates (HTTP, WebSocket, JSON-schema validation, " +
  "UUID, …) are not restricted; faithfulness decides.\n" +
  "- Credential storage is pluggable by the host application and never shells out.\n" +
  "\n" +
  "CONTEXT, NOT AUTHORITY: the documents under " + DOCS + " record earlier decisions, transport notes, and audit " +
  "findings. They may themselves be over-prescriptive or wrong. Use them as background only. Any deviation from " +
  "pi — whether it comes from a doc, from this prompt, or from existing Rust code — is either corrected to pi's " +
  "behavior or reported as a four-column row for the owner to judge; it is never followed silently. Section G of " +
  AUDIT + " lists observed deltas from pi that are still open.\n" +
  "\n" +
  "CONSTRAINTS:\n" +
  "- Work only inside " + REPO + "/ai (plus ai/Cargo.toml and the workspace Cargo.lock). Never modify " + PI + " " +
  "or " + REPO + "/genai. Never run cargo fmt on genai/.\n" +
  "- Layout mirrors pi file-for-file (src/<path>/<name>.rs <= src/<path>/<name>.ts; cli.ts becomes src/bin/pi-ai.rs); " +
  "lib.rs re-exports mirror index.ts.\n" +
  "- Comments only for constraints the code cannot show, plus pi file.ts:line provenance anchors.\n" +
  "- Tests are part of the port: port pi's " + PI + "/test cases for this phase's subjects, hermetically (no " +
  "network, no live keys). List live/e2e/smoke tests as skipped with a reason. Where pi has no unit test for a " +
  "behavior you port, pin it with a test that cites the pi source line.\n" +
  "- Gates: cargo fmt -p agentprism-ai; cargo clippy -p agentprism-ai --all-targets -- -D warnings; cargo test " +
  "-p agentprism-ai; cargo build --workspace; git diff --check. Never claim a gate you did not see pass; if the " +
  "sandbox denies a command or a crate fetch, say exactly which.\n" +
  "- Do not git commit; the workflow commits after approval.\n";

const PHASES = [
  {
    key: "q1",
    title: "Q1 · utils, auth core, store/catalog primitives, observed deltas",
    commitMsg: "ai: Q1 — port remaining utils, auth core, models-store, model-catalog, env-api-keys; resolve observed deltas",
    scope:
      "PHASE SCOPE (pi files → Rust; behavior comes from the pi source, not from this list):\n" +
      "- utils/abort.ts, utils/abort-signals.ts, utils/sleep.ts, utils/text.ts, utils/uuid.ts, utils/deferred-tools.ts, " +
      "utils/diagnostics.ts, utils/estimate.ts (as a public module), utils/retry.ts, utils/overflow.ts, " +
      "utils/validation.ts, utils/typebox-helpers.ts, utils/node-http-proxy.ts\n" +
      "- env-api-keys.ts, models-store.ts, model-catalog.ts\n" +
      "- auth/types.ts, auth/context.ts, auth/credential-store.ts, auth/helpers.ts, auth/resolve.ts\n" +
      "- Section G of " + AUDIT + ": each row is an observed delta between this crate and pi in already-ported " +
      "code. Resolve each to pi's behavior, or report it in the four-column table if it genuinely cannot be.\n" +
      "If the working tree contains uncommitted work from an earlier attempt at this phase, it is material, not " +
      "truth: audit it against pi like any other code, keep what is faithful, fix or replace what is not.",
  },
  {
    key: "q2",
    title: "Q2 · models.ts, catalog data, faux, openai-family provider factories",
    commitMsg: "ai: Q2 — port models.ts, embedded catalog data, faux provider, openai-family provider factories",
    scope:
      "PHASE SCOPE (pi files → Rust; behavior comes from the pi source, not from this list):\n" +
      "- models.ts (in full; the few helpers already in ai/src/models.rs are absorbed into it)\n" +
      "- models.generated.ts, providers/*.models.ts, providers/data/*.json and providers/data/.manifest.json (the " +
      "catalog data, embedded verbatim as it stands at the pin)\n" +
      "- providers/faux.ts\n" +
      "- every providers/<name>.ts whose API is one this crate already implements (openai-completions, " +
      "openai-responses, openai-codex-responses); providers whose API lands in a later phase are ported in that phase",
  },
  {
    key: "q3",
    title: "Q3 · anthropic-messages + its providers",
    commitMsg: "ai: Q3 — port anthropic-messages and the providers that use it",
    scope:
      "PHASE SCOPE (pi files → Rust; behavior comes from the pi source, not from this list):\n" +
      "- api/anthropic-messages.ts\n" +
      "- every providers/<name>.ts whose API is anthropic-messages (including providers that use it alongside an " +
      "API ported earlier)",
  },
  {
    key: "q4",
    title: "Q4 · google-shared, google-generative-ai, google-vertex + providers",
    commitMsg: "ai: Q4 — port google-shared, google-generative-ai, google-vertex and their providers",
    scope:
      "PHASE SCOPE (pi files → Rust; behavior comes from the pi source, not from this list):\n" +
      "- api/google-shared.ts, api/google-generative-ai.ts, api/google-vertex.ts\n" +
      "- every providers/<name>.ts whose API is google-generative-ai or google-vertex",
  },
  {
    key: "q5",
    title: "Q5 · bedrock-converse-stream + provider",
    commitMsg: "ai: Q5 — port bedrock-converse-stream, bedrock-provider, amazon-bedrock provider",
    scope:
      "PHASE SCOPE (pi files → Rust; behavior comes from the pi source, not from this list):\n" +
      "- api/bedrock-converse-stream.ts, bedrock-provider.ts\n" +
      "- every providers/<name>.ts whose API is bedrock-converse-stream",
  },
  {
    key: "q6",
    title: "Q6 · OAuth flows",
    commitMsg: "ai: Q6 — port auth/oauth flows, extension OAuth types, radius config",
    scope:
      "PHASE SCOPE (pi files → Rust; behavior comes from the pi source, not from this list):\n" +
      "- auth/oauth/*.ts (all files), compat/extension-oauth-types.ts, oauth.ts, bun-oauth.ts, providers/radius-config.ts",
  },
  {
    key: "q7",
    title: "Q7 · cloudflare, radius, all.ts, compat, legacy aliases, cli, final sweep",
    commitMsg: "ai: Q7 — port cloudflare binding and providers, radius provider, providers/all.ts, compat, legacy-api-aliases, cli",
    scope:
      "PHASE SCOPE (pi files → Rust; behavior comes from the pi source, not from this list):\n" +
      "- api/cloudflare-gateway-binding.ts; providers/cloudflare-auth.ts, providers/cloudflare-stream.ts, " +
      "providers/cloudflare-ai-gateway.ts, providers/cloudflare-workers-ai.ts; providers/radius.ts\n" +
      "- providers/all.ts, compat.ts, legacy-api-aliases.ts, cli.ts\n" +
      "FINAL SWEEP (this phase gates the whole port): compare " + PI + "/src against ai/src file-for-file and " +
      "pi's exports (index.ts, compat.ts, providers/all.ts, the public surface of each api/*.ts) export-for-export; " +
      "list each with its Rust counterpart, its owner-ruling exclusion, or its four-column row. Do the same for " +
      PI + "/test: every test file ported, skipped-with-reason, or excluded by ruling. Anything unaccounted for " +
      "is incomplete.",
  },
];

function implPrompt(ph, feedback, attempt) {
  return (
    "You are the IMPLEMENTER (" + IMPLEMENTER + ", xhigh reasoning) working in " + REPO + ".\n\n" +
    COMMON + "\n" + ph.scope + "\n\n" +
    (feedback
      ? "A REVIEWER REJECTED your previous attempt (round " + attempt + "). Your prior edits are still on disk; " +
        "address every point below without regressing what was already faithful:\n" + feedback + "\n\n"
      : "") +
    "Report, in plaintext: files added/changed; dependency decisions with the vendored-source verification; the " +
    "pi-test → Rust-test mapping (ported / pinned-from-source / skipped-with-reason); gate commands run and their " +
    "results; anything the sandbox denied; any place this prompt, a doc, or existing code disagreed with pi and what " +
    "you did; and the " +
    "four-column CANNOT-PRESERVE table — empty if nothing was left behind."
  );
}

function reviewPrompt(ph, implReport) {
  return (
    "You are the REVIEWER (" + REVIEWER + ", xhigh effort) in " + REPO + ".\n" +
    "You do not modify, create, stage, or commit files. You read, and you run verification commands.\n\n" +
    COMMON + "\n" + ph.scope + "\n\n" +
    "The implementer reported:\n" + (implReport == null ? "(no report)" : String(implReport)) + "\n\n" +
    "Judge the actual working-tree changes against pi's source, as an adversary looking for anything a pi-ai " +
    "consumer would notice:\n" +
    "1. Complete: every pi file and export in scope has a Rust counterpart; nothing deferred, stubbed, or left for " +
    "later.\n" +
    "2. Faithful: read the pi source for the scope and compare behavior — the port must match what pi does, " +
    "including in failure cases and in what it persists. List what you verified and how.\n" +
    "3. Honest about limits: judge every CANNOT-PRESERVE row on its own; a row Rust can achieve is a rejection.\n" +
    "4. Dependencies: confirm vendored-source verification claims yourself.\n" +
    "5. Tests: pi's tests for the scope are ported or accounted for; pins cite pi lines; everything hermetic.\n" +
    "6. Gates: run them yourself — cargo fmt -p agentprism-ai -- --check; cargo clippy -p agentprism-ai " +
    "--all-targets -- -D warnings; cargo test -p agentprism-ai; cargo build --workspace; git diff --check — and " +
    "confirm only ai/ (+ Cargo.toml/lock) changed.\n" +
    "ok=true only if the phase is complete and faithful with no blocking defect. Otherwise ok=false with specific, " +
    "file-and-line feedback — it is the implementer's only context next round. Where the phase scope, any doc, or " +
    "existing code disagrees with pi, pi is right; a deviation that is neither corrected nor reported as a row is a " +
    "rejection."
  );
}

const REVIEW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "feedback", "summary", "gatesRun", "blocking", "cannotPreserve"],
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
    cannotPreserve: {
      type: "array",
      description: "Rows the reviewer accepts as genuinely unpreservable, each judged on its own.",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["delta", "technicalDelta", "piCounterpart", "why"],
        properties: {
          delta: { type: "string" },
          technicalDelta: { type: "string" },
          piCounterpart: { type: "string" },
          why: { type: "string" },
        },
      },
    },
  },
};

const COMMIT_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["committed", "sha", "note"],
  properties: { committed: { type: "boolean" }, sha: { type: "string" }, note: { type: "string" } },
};

phase("Setup");
log("pi-ai full port v2 · impl=" + IMPLEMENTER + "/xhigh · rev=" + REVIEWER + "/xhigh · ≤" + MAX_ROUNDS + " rounds/phase · pi pin " + PIN.slice(0, 9));

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
    "In " + REPO + ", commit the phase's work as ONE commit, scoped to the crate:\n" +
      "  git add ai Cargo.lock\n  git commit -m '" + ph.commitMsg.replace(/'/g, "") + "'\n" +
      "Then read back the SHA with git rev-parse HEAD. If there is nothing staged, make no commit. Do not modify " +
      "any file, amend history, push, or touch other branches.",
    { label: "commit:" + ph.key, phase: ph.title, model: COMMITTER, mode: "bypassPermissions", cwd: REPO, schema: COMMIT_SCHEMA }
  );
  const c = commit || { committed: false, sha: "", note: "commit agent returned null" };
  const shaOk = typeof c.sha === "string" && /^[0-9a-f]{7,40}$/i.test(c.sha.trim());
  const sha = c.committed === true && shaOk ? c.sha.trim() : null;
  const rows = Array.isArray(verdict.cannotPreserve) ? verdict.cannotPreserve.length : 0;
  log("✔ " + ph.key + " approved after " + outcome.attempts + " round(s)" + (sha ? " · committed " + sha.slice(0, 9) : " · ⚠ no commit: " + c.note) + (rows ? " · cannot-preserve rows: " + rows : ""));
  results.push({ phase: ph.key, approved: true, rounds: outcome.attempts, sha: sha, summary: verdict.summary || "", cannotPreserve: verdict.cannotPreserve || [] });
}

return { pin: PIN, results: results };
