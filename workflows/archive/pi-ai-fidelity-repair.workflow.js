export const meta = {
  name: "pi-ai-fidelity-repair",
  description:
    "Repair every infidelity found by the 2026-08-20 independent audit of the openai-family port (audit doc in docs/). codex/gpt-5.6-sol (xhigh) implements; claude/opus[1m] (xhigh) reviews; sequential gated phases, commit per approved phase.",
  phases: [
    { title: "Setup" },
    { title: "R1 · protocol + types + helpers" },
    { title: "R2 · completions/responses substrate + fidelity" },
    { title: "R3 · codex" },
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
    key: "r1",
    title: "R1 · protocol + types + helpers",
    commitMsg: "ai: R1 — fidelity repairs in types, event protocol, and transform helpers (audit A2/A4/A5/A12 + C items)",
    scope:
      "SCOPE — resolve these audit items (files: ai/src/types.rs, ai/src/event_stream.rs, ai/src/api/transform_messages.rs, ai/src/utils/json_parse.rs as needed):\n" +
      "- A2: MessageBuilder tool-argument snapshots must reproduce pi's parseStreamingJson semantics exactly (repairJson pre-pass included, no invented depth cap, the parsed value pi would assign — not forced object-or-{}). The faithful implementation already exists in ai/src/utils/json_parse.rs; the contract is that a MessageBuilder snapshot mid-stream equals what pi's proxy reconstruction (proxy.ts:326 + json-parse.ts:104-124) would show for the same delta sequence.\n" +
      "- A4: inputs pi accepts must be accepted — a message with content: null or missing content lowers exactly as pi's transform-messages.ts:73 coercion to []. Whether you coerce at deserialization or in the transform is your choice; the observable contract is pi's (same accepted inputs, same lowered output, and serialization of such a message must not invent a field pi wouldn't emit).\n" +
      "- A5: tool-call-id sanitize/truncate/length must operate on UTF-16 code units as pi does (anthropic-messages.ts:1117, openai-completions.ts:1157-1158, openai-responses-shared.ts:148) — an astral char yields the same number of underscores and the same truncation boundary as pi. Note the crate already has a correct UTF-16-domain precedent in utils/sanitize_unicode.rs and openai_prompt_cache.rs got the code-point domain right for its own case; match each pi call site's actual domain.\n" +
      "- A12: the partial-free event protocol must carry enough for MessageBuilder snapshots to converge with pi's partial view at the same points in the stream: thinking signature accumulation (pi sets thinkingSignature at thinking_start and accumulates over signature deltas — anthropic-messages.ts:620-638,691-697), redacted thinking (redacted:true + pi's placeholder text at start), and toolCall namespace at toolcall_start (openai-responses-shared.ts:485-527). Extend the event enum/MessageBuilder accordingly (serde: new fields must be optional so existing wire consumers are unaffected); R2/R3 make the producers populate them — in THIS phase update any producer already in ai/ that pi sets at start-time, and keep all existing tests green.\n" +
      "- C (compat tolerance): a models.json entry pairing a compat block with a custom/unfamilied api string loads in pi (compat kept as inert data, no runtime validation — pi's check is compile-time only); Model deserialization must accept what pi accepts. Keep the family-match enforcement only where pi observably enforces it (i.e., don't reject at load).\n" +
      "- C (wire number formatting): whole-number f64 fields that pi puts on the WIRE (request bodies: temperature and friends) must serialize as pi's JSON.stringify does (1, not 1.0). Provide the serializer where wire structs need it; non-wire persistence fields follow the same rule where pi round-trips them byte-for-byte in session files.\n\n" +
      "OUT OF SCOPE (deliberately, already adjudicated): result() erroring on missing-terminal (pinned deliberate), BTreeMap header ordering, stack:None diagnostics, platform-forced parse-breadth differences — the audit doc section C lists them.",
  },
  {
    key: "r2",
    title: "R2 · completions/responses substrate + fidelity",
    commitMsg: "ai: R2 — completions/responses transport + fidelity repairs (audit A1/A3/A6/A7/A11 + B1-B4)",
    scope:
      "SCOPE — resolve these audit items (files: ai/src/api/openai_completions.rs, ai/src/api/openai_responses.rs, ai/src/api/openai_responses_shared.rs, plus test scaffolding):\n" +
      "- B1-B4 (substrate): the observable contract is pi's — exactly ONE Authorization line when the caller supplies a custom authorization header (openai-completions.ts:747-753); provider error strings byte-equal to pi's formatProviderError(normalizeProviderError(...)) including the raw JSON error body, param, and OpenRouter error.metadata.raw appendix (openai-completions.ts:672-681; openai-responses.ts:88-90 + error-body.ts); onResponse receives the REAL response status and headers (openai-completions.ts:319, openai-responses.ts:159); retry honors retry-after / retry-after-ms / x-should-retry and the maxRetryDelayMs fail-fast exactly as utils/provider_retry.rs implements from pi; SSE text deltas byte-correct when a multibyte char splits across network chunks; multi-line data: SSE events concatenated per the SSE spec as pi's SDK does. MECHANISM IS YOUR CHOICE — the audit doc section B records verified capabilities: openai_oxide::config::Config is a public trait (custom build_request controls auth), post_stream_json_bytes returns the real reqwest::Response, SseStream::new is public; and the in-crate codex transport (ai/src/api/openai_codex_responses/transport.rs) is the established precedent for a fully self-owned reqwest+SSE path using oxide for types only. Read the vendored oxide source (~/.cargo/registry/src/index.crates.io-*/openai-oxide-0.16.0) before choosing. Whatever you choose, request bytes must remain byte-identical to pi (existing wire tests must keep passing unmodified except where a test itself embodies a listed defect).\n" +
      "- A1: remove the model.api equality gate (openai_completions.rs:479, openai_responses.rs:328) — pi never gates on model.api (echo-only: openai-completions.ts:276, openai-responses.ts:115).\n" +
      "- A3: assistant wire message with tool calls and no text serializes content as explicit null exactly where pi does (openai-completions.ts:1225,1340-1348) — null-vs-omitted parity.\n" +
      "- A6: decode tolerance equals pi's: (a) completions chunk fields duck-typed as pi reads them — wrong-typed content/reasoning/usage fields are ignored, never a terminal error (openai-completions.ts:539-566,1491-1509); (b) responses item decoding — unknown message content-part types lower to empty text as pi does, unknown phase values ignored (openai-responses-shared.ts:699,748); (c) service_tier outside the known set passes through with multiplier 1 as resolveCodexServiceTier does, never a decode failure.\n" +
      "- A7: qwen thinkingLevelMap explicit-null entry — pi's nullish ?? sends reasoning_effort with the level name (openai-completions.ts:842); match pi per-branch null-vs-defined semantics exactly (zai already correct).\n" +
      "- A11: response.failed with empty-string incomplete reason falls to pi's 'Unknown error (no error details in response)' (openai-responses-shared.ts:748-753).\n" +
      "- Populate the R1 event fields where pi's producers set them (toolCall namespace at toolcall_start per openai-responses-shared.ts:485-527; thinking start/signature semantics where these modules produce thinking).\n" +
      "- stream()/stream_simple() must never panic synchronously (pi's lazyStream never throws): calling outside a Tokio runtime must yield the in-band terminal error path, not a tokio::spawn panic.\n" +
      "- TEST SCAFFOLDING: the hermetic server must preserve duplicate header lines (the current fold into a map hid B1) — add a regression test asserting exactly one Authorization line on the custom-auth path, and tests for real onResponse status/headers, pi-exact error strings for JSON-body and non-JSON-body failures, retry-after honored, split-multibyte SSE, and multi-line data: events.",
  },
  {
    key: "r3",
    title: "R3 · codex",
    commitMsg: "ai: R3 — codex fidelity repairs (audit A8/A9/A10)",
    scope:
      "SCOPE — resolve these audit items (files: ai/src/api/openai_codex_responses.rs, its transport.rs/tests.rs):\n" +
      "- A8: a WebSocket stream that completes cleanly but whose terminal maps to stop-reason error (response.incomplete with reason != max_output_tokens; terminal status failed/cancelled) must take pi's WS-failure path: append the provider_transport_failure diagnostic, record the WS failure (websocketFailures/lastWebSocketError), and pin the session to SSE fallback — exactly as pi's assertSuccessfulOutput inside the WS try does (openai-codex-responses.ts:325,348-363). In-band error/response.failed frames already do this; only the semantic-status case diverges. Add the missing test.\n" +
      "- A9: empty-body non-2xx error string uses pi's raw || statusText || 'Request failed' semantics (openai-codex-responses.ts:428-433,1551) — over HTTP/2 the reason phrase is empty, so the observable string is 'Request failed', never an IANA canonical phrase the wire didn't carry.\n" +
      "- A10: the retryable-pattern matcher reproduces JS `.?` semantics — the optional character excludes line terminators (\\n, \\r, U+2028, U+2029) per openai-codex-responses.ts:129.\n" +
      "- Populate any R1 event fields this module's producers set at start-time in pi.\n" +
      "- stream() must never panic synchronously (same contract as R2).",
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
