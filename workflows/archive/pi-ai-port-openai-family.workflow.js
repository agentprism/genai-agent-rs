export const meta = {
  name: "pi-ai-port-openai-family",
  description:
    "Port pi-ai's substrate and the openai-family API implementations (types+events, shared helpers, openai-completions on openai-oxide, openai-responses(+shared), codex) into the agentprism-ai crate. codex/gpt-5.6-sol (xhigh) implements; claude/opus[1m] (xhigh) reviews for pi-faithfulness; sequential gated phases, commit per approved phase.",
  phases: [
    { title: "Setup" },
    { title: "P1 · types + event stream" },
    { title: "P2 · shared helpers" },
    { title: "P3 · openai-completions" },
    { title: "P4 · openai-responses + shared" },
    { title: "P5 · codex" },
  ],
};

const REPO = "/Users/vikashloomba/genai-agent";
const PI = "/Users/vikashloomba/pi/packages/ai";
const PIN = "496185f6e4267b979e3663c45f7eb70b0c6a97b4";
const IMPLEMENTER = "codex/gpt-5.6-sol";
const REVIEWER = "claude/opus[1m]";
const COMMITTER = "claude/haiku";
const MAX_ROUNDS = 4;

const COMMON =
  "GOVERNING RULES (identical for every phase):\n" +
  "- The reference is pi-ai at " + PI + " (repo pinned at commit " + PIN + "). Faithful means a consumer " +
  "of the Rust `ai` crate observes no feature or behavior difference from pi-ai beyond language " +
  "semantics: same request shapes and field-presence semantics, same event sequences and ordering, same " +
  "error strings where observable, same usage/cost math, same defaults and fallbacks.\n" +
  "- Read " + REPO + "/docs/porting-pi-ai-and-agent-core-docs/v2/preserved-architectural-seams-pi-ai-v2.mdx " +
  "(binding seam rulings — notably seam #5: the partial-free event protocol is canonical, with a " +
  "MessageBuilder-style accumulator for snapshot consumers; seam #12: runtime failures are in-band " +
  "terminal error events, never Result::Err on stream methods; seam #10: Api/ProviderId are open " +
  "unions, never closed enums) and " +
  REPO + "/docs/porting-pi-ai-and-agent-core-docs/provider-api-implementations.mdx (verified transport " +
  "truths per module).\n" +
  "- Work ONLY inside " + REPO + "/ai (plus, if a cached dependency must be added, ai/Cargo.toml and the " +
  "workspace Cargo.lock). NEVER modify anything under " + PI + " or " + REPO + "/genai (the fork is " +
  "read-only donor material). Never run cargo fmt on the genai/ subtree.\n" +
  "- Scaffold contracts in ai/src (module docs, the ProviderStreams trait in api/mod.rs) are the agreed " +
  "seam; refine signatures where the port requires, but preserve the contracts they state. Replace " +
  "'PORT TARGET shell' items wholesale.\n" +
  "- Serde wire fidelity: the JSON these types serialize to IS pi's wire/session format — field names " +
  "(camelCase where pi uses camelCase), tagged unions on the same discriminants, omitted-vs-present " +
  "semantics preserved (Option + skip_serializing_if only where pi omits; presence-bearing fields must " +
  "distinguish unset from explicit zero/empty exactly as pi does).\n" +
  "- House rules: no unsafe; no lossy `as` casts where pi range-checks; serde_json::Value/Map only where " +
  "pi's value is genuinely arbitrary JSON (samplingParams, metadata, tool schemas, opaque fidelity " +
  "fields) — never as a shortcut around modeling a known shape.\n" +
  "- Code comments: only constraints the code cannot show, plus pi `file.ts:line` provenance anchors on " +
  "non-obvious behavior. No narration.\n" +
  "- Tests are part of the port: hermetic only (no network, no live keys). Port each in-scope pi test " +
  "case-for-case where one exists; where pi has no unit test for an observable behavior, pin it with a " +
  "test derived from the pi source (cite file:line in the test doc comment). pi tests that are live/e2e " +
  "are NOT ported — list each with a one-line reason in the phase report. pi tests that resolve models " +
  "from the generated catalog use inline `Model` literals reproducing the exact fields the test relies " +
  "on (note which catalog entry each mirrors).\n" +
  "- Gates you must run and make pass: cargo fmt -p agentprism-ai; cargo clippy -p agentprism-ai " +
  "--all-targets -- -D warnings; cargo test -p agentprism-ai; cargo build --workspace; git diff --check. " +
  "Use --offline if the sandbox denies network (the registry cache is pre-warmed; openai-oxide 0.16 and " +
  "the hyper test stack are already in Cargo.lock). If your sandbox denies a specific command, still " +
  "land the code, report the exact denied command, and let the reviewer run it — never claim a gate " +
  "passed that you did not see pass.\n" +
  "- Do NOT git commit; the workflow commits after review approval.\n";

const PHASES = [
  {
    key: "p1",
    title: "P1 · types + event stream",
    commitMsg: "ai: P1 — port pi types.ts and the event-stream contract (partial-free protocol + MessageBuilder)",
    scope:
      "SCOPE — port these pi sources into the ai crate:\n" +
      "1. " + PI + "/src/types.ts (858 lines) → ai/src/types.rs. The full type surface: Message union " +
      "(user/assistant/toolResult), content blocks (text/thinking/toolCall/image and the tool-result " +
      "content forms), AssistantMessage with ALL fidelity fields (api, provider, model, responseModel, " +
      "responseId, diagnostics, usage, stopReason, deferred, errorMessage, rawStopReason, endTurn, " +
      "timestamp; thinkingSignature/thoughtSignature/textSignature/reasoningDetails/redacted on content " +
      "as opaque round-trip payloads), Usage (+cacheWrite1h, reasoning subset), StopReason, Model + the " +
      "per-API compat families as an enum-per-family (OpenAICompletionsCompat, OpenAIResponsesCompat, " +
      "AnthropicMessagesCompat, BedrockCompat — variant must match model.api), Context, Tool (schema " +
      "data: serde_json::Value parameters), ProviderRequestOptions/StreamOptions/SimpleStreamOptions " +
      "two-tier options (headers value type must support the null-suppresses-default sentinel — " +
      "Option<String> values), Transport, CacheRetention, ThinkingLevel + thinkingLevelMap, " +
      "DeferredHandle (plain serde data), AssistantMessageDiagnostic, ProviderEnv, ProviderHeaders. " +
      "Api/ProviderId as open newtype strings.\n" +
      "2. " + PI + "/src/utils/event-stream.ts (88 lines) → ai/src/event_stream.rs, under the seam #5 " +
      "ruling: AssistantMessageEvent enum WITHOUT partial snapshots — contentIndex-addressed block " +
      "deltas exactly as pi's event vocabulary (start, text_start/text_delta/text_end, thinking_*, " +
      "toolcall_* with partial-argument deltas, done{message}, error{reason,error-message}) — plus an " +
      "AssistantMessageEventStream (Stream + terminal-authoritative result(), exactly-once terminal " +
      "settlement, fused after terminal, missing-terminal = protocol error, channel producer) and a " +
      "MessageBuilder accumulator that reconstructs the AssistantMessage snapshot from events (this " +
      "replaces pi's carried `partial`; pi's own proxy protocol in packages/agent/src/proxy.ts is the " +
      "precedent — match its reconstruction semantics). The fork's " + REPO + "/genai/src/assistant.rs, " +
      "assistant_stream.rs and assistant_accumulator.rs are permitted lift material (they are our owned " +
      "code, at pi 0.84.1 parity) — but audit every lifted line against current pi at " + PI + "; pi is " +
      "the authority, and the event enum must be reshaped to the partial-free canon.\n\n" +
      "TESTS (this phase): serde round-trip tests pinning the exact wire JSON of every message/content/" +
      "usage/diagnostic variant (field names, tags, omission behavior — cite types.ts lines per case); " +
      "event-stream primitive tests (terminal-authoritative result, exactly-once across clone/send " +
      "races, fused-after-terminal, missing-terminal protocol error, channel semantics); MessageBuilder " +
      "tests reconstructing messages from representative event sequences (text+thinking+parallel tool " +
      "calls with streamed argument deltas; done and error terminals). pi has no dedicated unit files " +
      "here — every test cites the pi source lines it pins.",
  },
  {
    key: "p2",
    title: "P2 · shared helpers",
    commitMsg: "ai: P2 — port pi shared helpers (utils closure, api helpers, transform-messages, cost/thinking)",
    scope:
      "SCOPE — port these pi sources (the full dependency closure of the openai-family modules):\n" +
      "- " + PI + "/src/utils/error-body.ts → ai/src/utils/error_body.rs (formatProviderError, " +
      "normalizeProviderError — error strings verbatim; body truncation semantics exact).\n" +
      "- " + PI + "/src/utils/hash.ts → ai/src/utils/hash.rs (shortHash/cyrb53 — pin with test vectors " +
      "computed from the pi implementation).\n" +
      "- " + PI + "/src/utils/headers.ts → ai/src/utils/headers.rs (headersToRecord and friends; " +
      "null-suppression semantics).\n" +
      "- " + PI + "/src/utils/json-parse.ts → ai/src/utils/json_parse.rs (parseStreamingJson partial-" +
      "parse matrix + parseJsonWithRepair — port pi's behavior exactly; implement the partial-JSON " +
      "recovery in-crate rather than adding an unvetted dependency).\n" +
      "- " + PI + "/src/utils/pi-user-agent.ts → ai/src/utils/pi_user_agent.rs.\n" +
      "- " + PI + "/src/utils/provider-env.ts → ai/src/utils/provider_env.rs (scoped-env-over-process-" +
      "env precedence; the Bun /proc/self/environ fallback is JS-runtime-specific — do not port it, " +
      "note it in the report).\n" +
      "- " + PI + "/src/utils/provider-retry.ts → ai/src/utils/provider_retry.rs (retryable statuses, " +
      "header-directed retries, retry-after parsing incl. HTTP-date and malformed→0, 60s server-delay " +
      "cap via maxRetryDelayMs with the classifiable immediate failure, jittered backoff, abortable " +
      "sleep, zero-retry default).\n" +
      "- " + PI + "/src/utils/sanitize-unicode.ts → ai/src/utils/sanitize_unicode.rs " +
      "(sanitizeSurrogates — careful: pi operates on UTF-16 lone surrogates; Rust strings are UTF-8, so " +
      "match pi's OBSERVABLE output for the same logical inputs; port test/unicode-surrogate.test.ts).\n" +
      "- " + PI + "/src/api/constrained-sampling.ts → ai/src/api/constrained_sampling.rs (+ port " +
      "test/constrained-sampling.test.ts).\n" +
      "- " + PI + "/src/api/github-copilot-headers.ts → ai/src/api/github_copilot_headers.rs.\n" +
      "- " + PI + "/src/api/openai-prompt-cache.ts → ai/src/api/openai_prompt_cache.rs.\n" +
      "- " + PI + "/src/api/simple-options.ts → ai/src/api/simple_options.rs (buildBaseOptions, " +
      "thinkingBudgetForLevel, clampThinkingBudgetToAnswerRoom — presence semantics exact).\n" +
      "- " + PI + "/src/api/transform-messages.ts → ai/src/api/transform_messages.rs. NOTE: the " +
      "scaffold stub currently sits at ai/src/utils/transform_messages.rs — MOVE it to api/ to mirror " +
      "pi, updating ai/src/utils/mod.rs and ai/src/api/mod.rs. This is seam #11's shared lowering pass; " +
      "pin ALL of: same-model verbatim thinking replay (signatures kept), cross-model thinking→" +
      "<thinking> text + redacted dropped, tool-call id normalization per target API constraint " +
      "(Anthropic ^[a-zA-Z0-9_-]+$ max 64; the 9-byte alnum scheme; Responses ids with | preserved " +
      "where pi preserves), errored/aborted assistant turns dropped, orphaned tool calls given " +
      "synthetic error results, images degraded to text placeholders for non-vision models — every " +
      "branch tested with pi line citations (pi has no dedicated unit file; derive from source).\n" +
      "- From " + PI + "/src/models.ts ONLY the pure helpers: calculateCost (tiered pricing via " +
      "inputTokensAbove; 2× base-input for 1h cache writes), clampThinkingLevel, " +
      "getSupportedThinkingLevels → ai/src/models.rs (module doc: grows into Models/Provider later). " +
      "Port test/provider-retry.test.ts case-for-case. utils/retry.ts and utils/overflow.ts (agent-tier " +
      "classifiers) are OUT of this phase's scope — do not port them yet.",
  },
  {
    key: "p3",
    title: "P3 · openai-completions",
    commitMsg: "ai: P3 — port openai-completions on openai-oxide (compat-parameterized, hermetic wire tests)",
    scope:
      "SCOPE — full port of " + PI + "/src/api/openai-completions.ts (1689 lines) → " +
      "ai/src/api/openai_completions.rs, implementing the ProviderStreams trait plus typed entry " +
      "points, on the ruled transport: openai-oxide 0.16 (already a dependency; source readable in " +
      "~/.cargo/registry/src/). openai-oxide plays exactly the role pi's `openai` SDK plays — client " +
      "construction (api key, base URL, default headers), request issue, SSE framing/chunk parsing — " +
      "with SDK-level retries disabled and pi's retryProviderRequest wrapping the initial call only " +
      "(chunk loop outside the wrapper, as in pi).\n" +
      "THE CONTRACT is the request bytes pi emits and the response fields pi reads, per compat branch " +
      "(thinkingFormat families incl. deepseek/zai/qwen/chat-template/string-thinking/ant-ling, " +
      "reasoning_content on replayed assistant messages, maxTokensField selection, presence-bearing " +
      "thinking-budget fields, samplingParams merged after named fields with caller keys winning, " +
      "session-affinity header formats, cacheControlFormat anthropic-style breakpoints). The MECHANISM " +
      "is your choice — read the library first (source: ~/.cargo/registry/src/index.crates.io-*/" +
      "openai-oxide-0.16.0). Verified capabilities: RequestOptions::extra_body(Value) shallow-merges " +
      "extra keys over the serialized typed body with extra winning (client.rs merge_body_json) — the " +
      "direct analogue of pi attaching dialect fields to the SDK params object and exactly pi's " +
      "samplingParams override semantics; RequestOptions::header/query/timeout are per-request; " +
      "chat().completions() and responses() each expose typed create/create_stream AND raw " +
      "create_raw/create_stream_raw (serde_json::Value in, SseStream<Value> out) for branches where " +
      "the RESPONSE must be read off-spec (reasoning_content/reasoning/reasoning_text deltas, " +
      "choice-level usage — typed chunks silently drop them; pi reads them deliberately). Choose per " +
      "branch; do not fight the crate, and do not bypass the typed path where the wire is on-spec.\n" +
      "Cover the entire module: buildParams for every compat flag in OpenAICompletionsCompat, message " +
      "conversion via transform_messages, tool wire shapes (+ grammar tools when " +
      "supportsOpenAIGrammarTools), streaming loop (text/reasoning deltas incl. reasoning_content " +
      "variants, tool-call chunk accumulation via parseStreamingJson, usage capture incl. the Kimi " +
      "top-level cached_tokens fallback without double-count, stop-reason mapping + rawStopReason, " +
      "responseModel/responseId capture), error surfacing via normalizeProviderError/" +
      "formatProviderError, onPayload/onResponse hooks, getPiUserAgent rules, Copilot dynamic headers, " +
      "prompt-cache key clamping, cache retention, deferred-tool (Kimi) same-request handling.\n\n" +
      "TESTS: port case-for-case, adapting pi's vi.mock(openai) pattern to a hermetic local HTTP server " +
      "(hyper dev-deps are wired) that captures the request JSON for assertions and replays scripted " +
      "SSE bytes through openai-oxide: openai-completions-cache-control-format, -empty-tools, " +
      "-prompt-cache, -raw-stop-reason, -reasoning-details, -response-model, -retry, -thinking-as-text, " +
      "-thinking-token-budget, -tool-choice, -tool-result-images (.test.ts each), plus " +
      "compat-env.test.ts if its subject is completions compat detection (else report where it " +
      "belongs). Catalog-driven model fixtures become inline Model literals. zai/qwen/baseten *-models " +
      "tests are generated-catalog tests — out of scope; list as deferred.",
  },
  {
    key: "p4",
    title: "P4 · openai-responses + shared",
    commitMsg: "ai: P4 — port openai-responses + openai-responses-shared (hermetic wire tests)",
    scope:
      "SCOPE — full port of " + PI + "/src/api/openai-responses-shared.ts (792 lines) → " +
      "ai/src/api/openai_responses_shared.rs (NO I/O: message/tool conversion into Responses input " +
      "shapes — foreign tool-call id handling, namespace tools, empty tool results, reasoning replay " +
      "shapes, item ids — plus processResponsesStream: the semantic event loop over decoded Responses " +
      "stream events producing our assistant events, incl. partial-JSON cleanup on arguments and " +
      "terminal-event semantics) and " + PI + "/src/api/openai-responses.ts (376 lines) → " +
      "ai/src/api/openai_responses.rs (buildParams with stream:true store:false, OpenAIResponsesCompat " +
      "branches, client via openai-oxide Responses streaming with SDK retries disabled and " +
      "retryProviderRequest on the initial call, session headers, onPayload/onResponse). The Responses " +
      "stream-event decoding must tolerate unknown event types the way the port needs for codex reuse: " +
      "an unknown event type must never be a hard deserialization failure that kills the stream. " +
      "Mechanism is your choice after reading openai-oxide (its responses() exposes typed " +
      "create/create_stream AND raw create_stream_raw yielding SseStream<serde_json::Value>; " +
      "RequestOptions carries per-request headers and extra_body with extra-wins merge semantics).\n\n" +
      "TESTS: port hermetically (same HTTP-server pattern): openai-responses-compat, " +
      "-empty-tool-result, -foreign-toolcall-id, -message-id, -namespace, -partial-json-cleanup, " +
      "-terminal-event, -tool-result-images (.test.ts each). The *-e2e tests (cache-affinity, " +
      "reasoning-replay) are live — list as skipped with reason.",
  },
  {
    key: "p5",
    title: "P5 · codex",
    commitMsg: "ai: P5 — port openai-codex-responses (lifted fork WS/SSE transport, collapsed to ai events)",
    scope:
      "SCOPE — full port of " + PI + "/src/api/openai-codex-responses.ts (1650 lines) → " +
      "ai/src/api/openai_codex_responses.rs (+ a private submodule tree as needed). This module is " +
      "hand-rolled in pi (types-only openai import) and stays hand-rolled here — NO openai-oxide on " +
      "this path. The fork's " + REPO + "/genai/src/codex/ (protocol.rs, token.rs, stream.rs, " +
      "events.rs, request.rs — a pi-line-cited port) is the designated lift material: copy what serves " +
      "(WS/SSE transport lifecycle, URL/header derivation incl. the responses_websockets beta header " +
      "handling, token/JWT accountId handling), adapt it into this crate, and COLLAPSE its internal " +
      "wire → ChatStreamEvent → assistant-event double hop: Codex wire events normalize into the same " +
      "decoded-Responses-event shape and flow through openai_responses_shared's processing directly. " +
      "Audit every lifted line against pi at " + PIN + ".\n" +
      "Must cover: transport selection sse/websocket/websocket-cached/auto (default auto), pre-stream " +
      "WS→SSE fallback with per-session fallback memory, the two single-retry WS cases (missing " +
      "continuation, connection limit), no SSE replay once WS events started, session socket cache " +
      "(5 min idle / 55 min total age) with cleanup registration, websocketConnectTimeoutMs vs " +
      "timeoutMs-as-idle-timeout, input-delta with previous_response_id on auto/websocket-cached vs " +
      "full context on websocket, the local SSE retry loop (429/5xx/text-classified + network errors, " +
      "Retry-After/Retry-After-Ms, exponential backoff, zero-retry default), auth/session headers and " +
      "originator, and zstd: pi compresses the SSE body at level 3 only when the runtime exposes " +
      "zstdCompressSync and sends plain JSON otherwise — if a zstd crate is not in the cached registry, " +
      "implement the uncompressed branch (behaviorally valid per pi's conditional) and note it in the " +
      "report; WS frames are always uncompressed. tokio-tungstenite 0.28 is cached (the fork uses it) — " +
      "add it to ai/Cargo.toml as needed.\n\n" +
      "TESTS: port openai-codex-stream.test.ts hermetically; add WS-path tests against a local " +
      "tokio-tungstenite server pinning: response.create frame shape, event normalization parity with " +
      "the SSE path, pre-stream fallback, and cache reuse across a session. openai-codex-oauth.test.ts " +
      "is the OAuth flow (out of scope — auth phase later); openai-codex-cache-affinity-e2e is live — " +
      "list both as skipped with reasons.",
  },
];

function implPrompt(phase, feedback, attempt) {
  return (
    "You are the IMPLEMENTER (" + IMPLEMENTER + ", xhigh reasoning) working in " + REPO + ".\n\n" +
    COMMON + "\n" + phase.scope + "\n\n" +
    (feedback
      ? "A REVIEWER REJECTED your previous attempt (round " + attempt + "). Your prior edits are still " +
        "on disk; fix every point below without regressing what was already correct:\n" + feedback + "\n\n"
      : "") +
    "Return a concise plaintext report: files added/changed; the pi-test → Rust-test mapping (ported / " +
    "pinned-from-source / skipped-with-reason); gate commands run with results; any sandbox-denied " +
    "commands; any pi behavior you could not port faithfully (do NOT silently deviate — report it)."
  );
}

function reviewPrompt(phase, implReport) {
  return (
    "You are the REVIEWER (" + REVIEWER + ", xhigh effort) in " + REPO + ".\n" +
    "STRICT RULE: do NOT modify, create, stage, or commit any file. Read and run verification commands " +
    "only (cargo fmt --check variants, clippy, cargo test, cargo build, git status/diff).\n\n" +
    "The phase under review:\n" + phase.scope + "\n\n" + COMMON + "\n" +
    "The implementer reported:\n" + (implReport == null ? "(no report)" : String(implReport)) + "\n\n" +
    "Judge the ACTUAL working-tree changes (git status + git diff + reading the files) against the pi " +
    "sources at " + PI + " with the rigor of a faithfulness audit:\n" +
    "1. BEHAVIOR: for each ported function, compare against the pi source line-by-line for observable " +
    "behavior — request field construction and omission semantics, compat branches, event ordering, " +
    "error strings, usage math, defaults, fallbacks. Spot-verify at least 15 substantive behaviors " +
    "against pi file:line and list each check you made.\n" +
    "2. SEAMS: stream methods return streams, never Result (failures in-band as terminal error events); " +
    "partial-free event protocol + MessageBuilder; Api/ProviderId open unions; two-tier options; " +
    "headers null-suppression sentinel; opaque fidelity fields round-trip via serde untouched; " +
    "samplingParams/metadata as untyped passthrough maps.\n" +
    "3. TESTS: verify the pi-test mapping is complete for the phase scope — every in-scope pi test " +
    "ported case-for-case or explicitly skipped with a sound reason; source-derived pins carry pi " +
    "line citations; all tests hermetic (no network).\n" +
    "4. GATES: independently run cargo fmt -p agentprism-ai -- --check (or equivalent), cargo clippy -p " +
    "agentprism-ai --all-targets -- -D warnings, cargo test -p agentprism-ai, cargo build --workspace, " +
    "git diff --check. Also confirm nothing under " + REPO + "/genai or " + PI + " was modified " +
    "(git status must show changes only under ai/ and, if justified, ai/Cargo.toml + Cargo.lock).\n" +
    "Set ok=true ONLY if the phase is genuinely faithful and complete with no blocking defect. " +
    "Otherwise ok=false with complete, file-and-line-specific feedback — it is the implementer's ONLY " +
    "context next round."
  );
}

const REVIEW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "feedback", "summary", "gatesRun", "blocking"],
  properties: {
    ok: { type: "boolean", description: "true only if the phase is faithful and complete with no blocking defect." },
    feedback: { type: "string", description: "Actionable fixes for the next round; empty when ok=true. Complete and specific (files, lines, exact pi references)." },
    summary: { type: "string", description: "One-paragraph verdict grounded in the diff and the pi sources you compared." },
    gatesRun: {
      type: "array",
      description: "Verification commands actually run.",
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
      description: "Blocking defects; empty when ok=true.",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["file", "issue"],
        properties: {
          file: { type: "string" },
          issue: { type: "string" },
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
    committed: { type: "boolean", description: "true only if a NEW commit was created." },
    sha: { type: "string", description: "git rev-parse HEAD after committing; empty if none." },
    note: { type: "string", description: "What was committed, or why nothing was." },
  },
};

phase("Setup");
log("pi-ai openai-family port · impl=" + IMPLEMENTER + "/xhigh · rev=" + REVIEWER + "/xhigh · ≤" + MAX_ROUNDS + " rounds/phase · pi pin " + PIN.slice(0, 9));

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

return { pin: PIN, results: results };
