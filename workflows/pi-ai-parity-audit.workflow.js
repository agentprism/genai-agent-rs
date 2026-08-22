export const meta = {
  name: "pi-ai-parity-audit",
  description:
    "Audit the agentprism-ai crate against docs/porting-pi-ai-and-agent-core-docs/goal.md — idiomatic Rust with full behavioral parity to pi-ai at the pin — and produce an adversarially verified findings report plus an ordered remediation plan. codex/gpt-5.6-sol at xhigh throughout; every agent is read-only except the final report writer.",
  model: "codex/gpt-5.6-sol",
  phases: [
    { title: "Setup" },
    { title: "Audit" },
    { title: "Verify" },
    { title: "Plan" },
  ],
};

const A = typeof args === "string" ? JSON.parse(args) : args || {};
const REPO = A.repo || "/home/vikash/genai-agent/genai-agent-rs";
const PI = A.pi || "/home/vikash/pi-pin-c49906ec7/packages/ai";
const PIN = A.pin || "c49906ec77788625aacbdc53ebca6fbe65bd20f5";
const PREVIOUS_PIN = A.previousPin || "496185f6e4267b979e3663c45f7eb70b0c6a97b4";
const DATE = A.date || "undated";
const GOAL = REPO + "/docs/porting-pi-ai-and-agent-core-docs/goal.md";
const REPORT = REPO + "/docs/porting-pi-ai-and-agent-core-docs/parity-audit-" + DATE + ".md";
const MODEL = "codex/gpt-5.6-sol";
const XHIGH = { reasoning_effort: "xhigh" };
const REFUTERS_FOR_SEVERE = 2;

const COMMON =
  "GOAL (the owner's, and the only standard that matters): read " + GOAL + " in full before anything else. " +
  "In short — the agentprism-ai crate at " + REPO + "/ai must BE pi-ai: every feature and every observable " +
  "behavior of pi-ai exists in the crate (public surface, the event protocol including what each event carries, " +
  "what is sent to providers, how responses/errors/retries/aborts/hooks behave, what is persisted and how it reads " +
  "back, the catalog, auth resolution, OAuth flows), written as idiomatic Rust (ownership, Result, traits, async, " +
  "real types), never impersonating the JS runtime or JS SDKs (no spawning node, no fabricated runtime versions, " +
  "no JS-isms kept for their own sake; truthful identity on the wire). Litmus: any pi-ai README example or test " +
  "must be recreatable against the crate with the same observable results, without a workaround and without " +
  "reading the crate internals. When idiom and parity seem to conflict, parity wins and idiom adapts; a feature is " +
  "never dropped for efficiency, convenience, or because an earlier design note said so. Internals are free " +
  "(SDK framing, HTTP stack, how a snapshot is produced); silent deviation is not. Anything genuinely " +
  "unpreservable in Rust is a four-column row — Delta | Technical delta | pi counterpart (file:line) | Why it " +
  "cannot be preserved — for the owner to judge; any row Rust can in fact achieve is a defect, not a row.\n\n" +
  "AUTHORITY: pi-ai source at " + PI + " (repo pinned at " + PIN + ") is the only authority. The crate was " +
  "ported against the previous pin " + PREVIOUS_PIN + "; everything pi changed between the two pins is in " +
  "scope as ordinary parity work. Documents under " + REPO + "/docs are background, never authority: where a doc " +
  "disagrees with pi it is wrong. Existing Rust code is what is being audited, not evidence of correctness. " +
  "Comments in the Rust code that cite pi file:line are claims to check, not facts.\n\n" +
  "OWNER RULINGS (fixed): not ported — api/lazy.ts and *.lazy.ts (lazy module loading), azure-openai-responses, " +
  "mistral-conversations, pi-messages, image generation (openrouter-images, images*.ts, image-models*, " +
  "images-api-registry, providers/images/*, providers/openrouter-images.ts), the agent package's proxy protocol, " +
  "Windows. SDK choices are decided: openai-completions/openai-responses → openai-oxide; anthropic-messages → " +
  "adk-anthropic; google-generative-ai/google-vertex → adk-gemini; bedrock-converse-stream → " +
  "aws-sdk-bedrockruntime + aws-config. The bar for SDK-backed modules is observable equivalence with what pi does " +
  "with its SDK (what is sent, how responses/errors/retries/hooks are handled); the SDK's own internals and framing " +
  "are free. Credential storage is pluggable by the host application and never shells out.\n\n" +
  "WHAT IS A FINDING: a concrete, source-anchored difference a consumer of the crate could observe versus pi " +
  "(missing feature or export, behavior delta, wire delta, persisted-data delta, error-path delta), a place where " +
  "the crate impersonates the JS runtime or an SDK or otherwise is not idiomatic Rust, a pi test or README example " +
  "that cannot be recreated against the crate as-is, or a doc/comment in the crate that would mislead a future " +
  "agent. Every finding cites pi file:line and Rust file:line you actually read. Read the pi files in scope in " +
  "full; do not sample. Do not report style preferences that change nothing observable. Do not report the owner " +
  "rulings above. Severity: blocking = a consumer-visible feature or behavior is missing or wrong, or the crate " +
  "impersonates a runtime/SDK; major = an observable delta in an edge or error path, a missing test that pins a " +
  "ported behavior, or a misleading doc/comment that contradicts pi; minor = cosmetic observable text differences " +
  "(key order, number formatting) and ergonomic/idiom issues with no behavior change.\n\n" +
  "YOU DO NOT MODIFY FILES. You read, grep, and may run read-only commands (cargo metadata, cargo doc --no-deps " +
  "is fine; do not run cargo test or builds that write outside target/ — this phase is read-only by design).\n";

const SCOPES = [
  {
    key: "types-events",
    title: "types.ts, utils/event-stream.ts, session-resources.ts — the data model and event protocol",
    files: "pi: types.ts, utils/event-stream.ts, session-resources.ts, index.ts (what it exports from these). Rust: ai/src/types.rs, ai/src/event_stream.rs, ai/src/session_resources.rs, ai/src/lib.rs re-exports.",
    focus:
      "Every exported type, union member, field, optional-vs-required, default, and helper; the AssistantMessageEvent " +
      "union member by member INCLUDING the `partial: AssistantMessage` snapshot pi carries on every nonterminal event " +
      "(the crate currently omits it — confirm and file it); Usage/cost semantics; JSON serialization shape of every " +
      "type as a consumer persisting and reloading context would see it (field names, key order, number formatting, " +
      "undefined-vs-null); the stream/result settlement semantics of pi's AssistantMessageEventStream.",
  },
  {
    key: "utils",
    title: "utils/* — shared utilities",
    files: "pi: every file under utils/ except event-stream.ts. Rust: ai/src/utils/*.rs.",
    focus:
      "Function-by-function behavior, inputs, outputs, error text; retry/backoff timing; provider-retry header parsing; " +
      "error-body truncation; json-parse partial semantics; overflow/estimate; validation messages; uuid v7 shape; " +
      "pi-user-agent (the crate must not spawn `node`, fabricate a Node version, or claim to be the OpenAI JS SDK — " +
      "pi's own intentional headers such as `User-Agent: pi (...)` are required; SDK telemetry headers such as " +
      "X-Stainless-* are the JS SDK's, not pi's — report current behavior and recommend the truthful option); " +
      "node-http-proxy (what pi does, what the idiomatic Rust equivalent is).",
  },
  {
    key: "models-catalog",
    title: "models.ts, model-catalog.ts, models-store.ts, models.generated.ts, env-api-keys.ts, compat.ts, legacy-api-aliases.ts, index.ts, providers/all.ts, providers/faux.ts, cli.ts",
    files: "pi: the listed files. Rust: ai/src/models.rs, model_catalog.rs, models_store.rs, models_generated.rs, env_api_keys.rs, compat/*, legacy_api_aliases.rs, lib.rs, providers/all.rs, providers/faux.rs, providers/data/*, src/bin/pi-ai.rs.",
    focus:
      "The Models collection API (create/set/get/refresh/stream/complete/streamSimple/completeSimple, auth " +
      "resolution order, transform headers, ModelsStore persistence format and read-back, refresh semantics and " +
      "publication ordering); every index.ts and compat.ts export has a Rust counterpart with the same semantics; " +
      "catalog contents versus pi's published 0.84.2 data (scratch copy at " +
      "/tmp/claude-1000/-home-vikash-genai-agent-genai-agent-rs/baa530f5-cd0c-4f9c-a0e7-9b22257ad504/scratchpad/pi-ts/node_modules/@earendil-works/pi-ai/dist/providers/data if present); " +
      "faux provider behavior; cli.ts behaviors in the bin.",
  },
  {
    key: "auth-oauth",
    title: "auth/*, auth/oauth/*, oauth.ts, bun-oauth.ts, compat/extension-oauth-types.ts",
    files: "pi: the listed files. Rust: ai/src/auth/**, oauth.rs, bun_oauth.rs, compat/extension_oauth_types.rs.",
    focus:
      "Credential types and JSON shapes as persisted; resolution order and error text; every OAuth flow step by step " +
      "(URLs, parameters, PKCE, device code polling intervals and error handling, callback server routes and pages, " +
      "token refresh, expiry math including JS falsiness cases); the credential store trait and its host-pluggable " +
      "contract; what happens with malformed token responses.",
  },
  {
    key: "openai-completions",
    title: "api/openai-completions.ts, openai-prompt-cache.ts, simple-options.ts, transform-messages.ts, constrained-sampling.ts, github-copilot-headers.ts",
    files: "pi: the listed files (openai-completions.ts changed between the pins — diff it). Rust: ai/src/api/openai_completions.rs, openai_prompt_cache.rs, openai_sse.rs, simple_options.rs, transform_messages.rs, constrained_sampling.rs, github_copilot_headers.rs.",
    focus:
      "Request construction field by field and key order; compat detection by URL; every stream event mapping " +
      "(text, reasoning, tool calls, usage, finish reasons, raw stop reason, response model/id); error paths " +
      "(HTTP errors, malformed JSON, aborted mid-stream, retries, error body passthrough); hooks (onPayload, " +
      "onResponse, fetch injection, headers/transformHeaders); what pi's SDK usage implies for the Rust transport " +
      "(timeouts, retry counts, SDK-level validation such as the timeout integer check).",
  },
  {
    key: "openai-responses-codex",
    title: "api/openai-responses.ts, openai-responses-shared.ts, openai-codex-responses.ts",
    files: "pi: the listed files. Rust: ai/src/api/openai_responses.rs, openai_responses_shared.rs, openai_codex_responses.rs, openai_codex_responses/transport.rs.",
    focus:
      "Same depth as openai-completions: request shape, item/event mapping, reasoning replay, namespaces, message " +
      "ids, partial JSON cleanup, terminal events, the codex WebSocket/SSE transport behaviors, cache affinity, " +
      "abort ordering and messages, error passthrough.",
  },
  {
    key: "anthropic",
    title: "api/anthropic-messages.ts and every provider whose api is anthropic-messages",
    files: "pi: api/anthropic-messages.ts, providers/anthropic.ts, and each providers/*.ts using anthropic-messages. Rust: ai/src/api/anthropic_messages.rs and the matching providers/*.rs.",
    focus:
      "Request construction (system, cache control, thinking/effort/adaptive, tools incl. eager input compat, " +
      "temperature compat, tool-name normalization, beta headers, OAuth vs API-key auth), SSE event mapping, " +
      "signatures, redacted thinking, usage incl. 1h cache write cost, errors/retries, the injected-client contract.",
  },
  {
    key: "google",
    title: "api/google-shared.ts, google-generative-ai.ts, google-vertex.ts and their providers",
    files: "pi: the listed files plus providers/google.ts, providers/google-vertex.ts. Rust: ai/src/api/google_shared.rs, google_generative_ai.rs, google_vertex.rs and providers/google*.rs.",
    focus:
      "Tool conversion, thinking level map/signature handling, signed empty blocks, image tool-result routing, raw " +
      "stop reasons, retry behavior, vertex routing/auth resolution (API key vs ADC vs service account), request " +
      "shape and response mapping, errors.",
  },
  {
    key: "bedrock",
    title: "api/bedrock-converse-stream.ts, bedrock-provider.ts, providers/amazon-bedrock.ts",
    files: "pi: the listed files. Rust: ai/src/api/bedrock_converse_stream.rs (+ tests), bedrock_provider.rs, providers/amazon_bedrock.rs.",
    focus:
      "Message conversion, thinking payloads, credential/endpoint/region resolution, custom and response headers, " +
      "redacted reasoning lifecycle, stop reasons, usage, error metadata, diagnostics text.",
  },
  {
    key: "cloudflare-radius",
    title: "api/cloudflare-gateway-binding.ts, api/cloudflare.ts, providers/cloudflare-*.ts, providers/radius.ts, providers/radius-config.ts",
    files: "pi: the listed files. Rust: ai/src/api/cloudflare_gateway_binding.rs, cloudflare.rs, providers/cloudflare_*.rs, providers/radius.rs, radius_config.rs.",
    focus:
      "Binding request validation and header stripping, stream behavior, auth, the Radius catalog refresh and " +
      "persistence including what happens with unusual model entries (pi keeps anything passing its shallow check).",
  },
  {
    key: "providers",
    title: "every remaining providers/*.ts factory and *.models.ts catalog module",
    files: "pi: providers/*.ts not covered above (openai, openai-codex, deepseek, groq, cerebras, xai, openrouter, together, baseten, huggingface, fireworks, github-copilot, kimi-coding, minimax*, moonshotai*, mistral, nvidia, opencode*, qwen-token-plan*, vercel-ai-gateway, xiaomi*, zai*, ant-ling) and their *.models.ts. Rust: the matching ai/src/providers/*.rs and *_models.rs.",
    focus:
      "Per factory: id, name, baseUrl, api, auth (env var names and order), headers, compat settings, model lists " +
      "and per-model fields; any factory options; provider-specific hooks. Report every mismatch per provider.",
  },
  {
    key: "readme-recreatability",
    title: "README.md recreatability — every code example in pi-ai's README",
    files: "pi: " + PI + "/../README.md (every ## and ### section with a code block; skip sections covered by the owner rulings: Image Generation, Browser Usage, Bundling and Tree Shaking, pi-messages). Rust: the crate's public surface (ai/src/lib.rs and what it re-exports) and ai/examples/quickstart.rs as an existing recreation.",
    focus:
      "For EACH README code example, write down (in your findings' evidence) the Rust equivalent you would write " +
      "against the crate's public surface and whether it exists and behaves the same. Anything that needs a " +
      "workaround (such as reconstructing event.partial with a builder), a missing export, a different semantic " +
      "(e.g. where pi accepts optional arguments and the crate needs boilerplate that changes meaning), or a " +
      "different observable result is a finding. Produce one finding per failing example, titled by README section.",
  },
  {
    key: "tests",
    title: "test/*.test.ts coverage — every pi test's Rust counterpart",
    files: "pi: " + PI + "/../test/*.test.ts (skip tests for owner-ruled-out features and live/e2e/smoke tests — list those as skipped with the reason). Rust: #[cfg(test)] modules and ai/tests/.",
    focus:
      "For each pi test file: is each test case ported (name the Rust test), pinned differently, or missing? A " +
      "missing hermetic test for a ported behavior is a major finding listing the exact cases. Also note Rust tests " +
      "that pin behavior contradicting pi (they are evidence of a behavior delta — file those as such).",
  },
  {
    key: "idiom-impersonation",
    title: "idiomatic-Rust and impersonation sweep across ai/src",
    files: "Rust: all of ai/src. pi: consult as needed.",
    focus:
      "Grep-driven sweep for: std::process::Command or any process spawning; fabricated runtime/SDK identity " +
      "(claiming Node versions, JS SDK versions, X-Stainless-* telemetry); unsafe; panics/unwrap/expect reachable " +
      "from library paths on user input; stringly-typed errors where pi has structured ones; JS-isms kept for their " +
      "own sake (null-as-default hacks, string-typed enums) versus JS semantics that ARE observable and must be kept " +
      "(falsiness-dependent behavior pi exposes); public API ergonomics that force non-pi meaning (e.g. required " +
      "option structs where pi has optional params — note which are fine idiom and which change behavior); " +
      "doc comments and source comments that cite docs or design decisions contradicting the goal (for example " +
      "'the canonical Rust event wire omits partial'). Each item: file:line, why it violates the goal, the " +
      "idiomatic fix that preserves parity.",
  },
];

const FINDINGS_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["area", "coverage", "findings", "cannotPreserveCandidates"],
  properties: {
    area: { type: "string", description: "The scope key you audited" },
    coverage: {
      type: "string",
      description: "Which pi files and Rust files you read in full, and anything in scope you could not cover (say so explicitly)",
    },
    findings: {
      type: "array",
      description: "Every source-anchored difference from the goal; empty only if you found none after reading everything in scope",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["title", "kind", "severity", "pi", "rust", "evidence", "remediation", "estimate"],
        properties: {
          title: { type: "string", description: "One line naming the observable difference" },
          kind: {
            type: "string",
            enum: ["missing-feature", "behavior-delta", "wire-delta", "persisted-data-delta", "error-path-delta", "runtime-impersonation", "non-idiomatic", "test-gap", "doc-misleading", "readme-not-recreatable"],
            description: "Classification",
          },
          severity: { type: "string", enum: ["blocking", "major", "minor"], description: "Per the COMMON definitions" },
          pi: { type: "string", description: "pi file:line(s) you read, e.g. types.ts:537-546" },
          rust: { type: "string", description: "Rust file:line(s) you read, e.g. ai/src/event_stream.rs:19-90; 'absent' if missing" },
          evidence: { type: "string", description: "What pi does versus what the crate does, concretely; quote the decisive lines" },
          remediation: { type: "string", description: "The idiomatic Rust change that restores parity, specific enough to implement; or a four-column row if genuinely unpreservable" },
          estimate: { type: "string", enum: ["small", "medium", "large"], description: "small: <1h of focused work; medium: a module-level change; large: cross-cutting" },
        },
      },
    },
    cannotPreserveCandidates: {
      type: "array",
      description: "Four-column rows for differences you believe Rust genuinely cannot achieve — rare; default to a finding",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["delta", "technicalDelta", "piCounterpart", "why"],
        properties: {
          delta: { type: "string", description: "Delta" },
          technicalDelta: { type: "string", description: "Technical delta" },
          piCounterpart: { type: "string", description: "pi counterpart file:line" },
          why: { type: "string", description: "Why it cannot be preserved in Rust" },
        },
      },
    },
  },
};

const VERDICT_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["verdict", "severity", "reason"],
  properties: {
    verdict: { type: "string", enum: ["confirmed", "refuted"], description: "confirmed = the difference is real per the sources; refuted = the sources show no such difference, or it is covered by an owner ruling" },
    severity: { type: "string", enum: ["blocking", "major", "minor"], description: "Your own severity per the COMMON definitions (may differ from the auditor's)" },
    reason: { type: "string", description: "The decisive pi and Rust lines you read and what they show; short" },
  },
};

const PLAN_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["reportPath", "summary", "packages"],
  properties: {
    reportPath: { type: "string", description: "Absolute path of the report you wrote" },
    summary: { type: "string", description: "Five lines: counts by severity and kind, the headline gaps" },
    packages: {
      type: "array",
      description: "Ordered remediation work packages; dependency order (foundations such as the event protocol first)",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["id", "title", "findingIds", "files", "scope", "acceptance", "estimate"],
        properties: {
          id: { type: "string", description: "Short stable id, e.g. P01" },
          title: { type: "string", description: "One line" },
          findingIds: { type: "array", items: { type: "string" }, description: "Finding ids from the report that this package resolves" },
          files: { type: "array", items: { type: "string" }, description: "Rust files expected to change (repo-relative)" },
          scope: { type: "string", description: "What to change and why, with pi file:line anchors — self-contained enough for an implementer with no other context" },
          acceptance: { type: "string", description: "Observable checks a reviewer runs: pi tests to port/pass, README examples that must now be recreatable verbatim, wire/persisted shapes to compare" },
          estimate: { type: "string", enum: ["small", "medium", "large"], description: "Size" },
        },
      },
    },
  },
};

function auditPrompt(scope) {
  return (
    "You are a pi-ai parity AUDITOR (" + MODEL + ", xhigh reasoning). Working directory: " + REPO + ".\n\n" +
    COMMON + "\n" +
    "YOUR SCOPE: " + scope.title + "\n" +
    "FILES: " + scope.files + "\n" +
    "FOCUS: " + scope.focus + "\n\n" +
    "Method: read every pi file in scope in full first; then the Rust counterparts in full; then compare " +
    "behavior by behavior, not file by file. For anything the Rust code claims via a pi file:line comment, open " +
    "that pi line and check. Where pi changed between the pins (" + PREVIOUS_PIN.slice(0, 9) + " → " +
    PIN.slice(0, 9) + "), use `git -C " + PI + "/../../.. diff " + PREVIOUS_PIN + " " + PIN + " -- packages/ai` " +
    "and treat unported changes as findings. Return the structured result; put everything you read in coverage."
  );
}

function refutePrompt(finding, area) {
  return (
    "You are an adversarial VERIFIER (" + MODEL + ", xhigh reasoning). Working directory: " + REPO + ".\n\n" +
    COMMON + "\n" +
    "An auditor of scope '" + area + "' filed this finding:\n" + JSON.stringify(finding, null, 2) + "\n\n" +
    "Try to REFUTE it: open the cited pi lines and Rust lines (and whatever else is needed), and decide whether " +
    "the claimed difference is real, observable by a crate consumer, and not covered by an owner ruling. Default " +
    "to 'refuted' only when the sources actually contradict the finding; a finding that is real but mis-cited is " +
    "confirmed with the corrected lines in your reason. Assign your own severity per the definitions."
  );
}

phase("Setup");
log("pi-ai parity audit · " + SCOPES.length + " auditors · pin " + PIN.slice(0, 9) + " (previous " + PREVIOUS_PIN.slice(0, 9) + ") · " + MODEL + "/xhigh · read-only");

phase("Audit");
const audits = (
  await parallel(
    SCOPES.map((scope) => () =>
      agent(auditPrompt(scope), {
        label: "audit:" + scope.key,
        phase: "Audit",
        model: MODEL,
        mode: "read-only",
        cwd: REPO,
        configOptions: XHIGH,
        schema: FINDINGS_SCHEMA,
        retries: 1,
      })
    )
  )
).filter(Boolean);
const failedScopes = SCOPES.filter((scope) => !audits.some((audit) => audit.area === scope.key)).map((scope) => scope.key);
if (failedScopes.length) log("⚠ auditors returned nothing for: " + failedScopes.join(", "));

let counter = 0;
const raw = audits.flatMap((audit) =>
  audit.findings.map((finding) => ({ id: "F" + String(++counter).padStart(3, "0"), area: audit.area, ...finding }))
);
const candidates = audits.flatMap((audit) => audit.cannotPreserveCandidates.map((row) => ({ area: audit.area, ...row })));
log("audit: " + raw.length + " raw findings (" +
  raw.filter((f) => f.severity === "blocking").length + " blocking, " +
  raw.filter((f) => f.severity === "major").length + " major, " +
  raw.filter((f) => f.severity === "minor").length + " minor) · " + candidates.length + " cannot-preserve candidates");

phase("Verify");
const verified = (
  await pipeline(
    raw,
    (finding) =>
      parallel(
        Array.from({ length: finding.severity === "minor" ? 1 : REFUTERS_FOR_SEVERE }, (_, index) => () =>
          agent(refutePrompt(finding, finding.area), {
            label: "verify:" + finding.id + (index ? ":b" : ""),
            phase: "Verify",
            model: MODEL,
            mode: "read-only",
            cwd: REPO,
            configOptions: XHIGH,
            schema: VERDICT_SCHEMA,
            retries: 1,
          })
        )
      ),
    (votes, finding) => {
      const cast = votes.filter(Boolean);
      const refuted = cast.length > 0 && cast.every((vote) => vote.verdict === "refuted");
      const severities = cast.map((vote) => vote.severity);
      const rank = { blocking: 3, major: 2, minor: 1 };
      const severity = severities.length
        ? severities.reduce((best, current) => (rank[current] > rank[best] ? current : best), severities[0])
        : finding.severity;
      return { ...finding, confirmed: !refuted, verifierSeverity: severity, votes: cast };
    }
  )
).filter(Boolean);
const confirmed = verified.filter((finding) => finding.confirmed);
const refuted = verified.filter((finding) => !finding.confirmed);
log("verify: " + confirmed.length + " confirmed, " + refuted.length + " refuted");

phase("Plan");
const plan = await agent(
  "You are the SYNTHESIZER (" + MODEL + ", xhigh reasoning). Working directory: " + REPO + ".\n\n" +
    COMMON + "\n" +
    "You MAY write exactly one file: " + REPORT + " (create it; overwrite if present). Do not modify anything else.\n\n" +
    "Inputs — confirmed findings (" + confirmed.length + "):\n" + JSON.stringify(confirmed.map(({ votes, ...f }) => f), null, 1) + "\n\n" +
    "Refuted findings, for the record (" + refuted.length + "):\n" + JSON.stringify(refuted.map((f) => ({ id: f.id, title: f.title, reason: (f.votes[0] || {}).reason || "" })), null, 1) + "\n\n" +
    "Cannot-preserve candidates from auditors (judge each yourself against pi and Rust; most are achievable and become findings):\n" + JSON.stringify(candidates, null, 1) + "\n\n" +
    "Auditor coverage statements:\n" + audits.map((a) => "- " + a.area + ": " + a.coverage).join("\n") + "\n\n" +
    "Write the report in Markdown with: (1) a header naming the goal doc, the pin " + PIN + ", the previous pin, the date " + DATE + ", and the method; " +
    "(2) a summary with counts by severity and kind; (3) a findings table — id, severity (use the verifier severity), kind, title, pi, rust, evidence (short), remediation — merging obvious duplicates across areas into one row that lists all original ids; " +
    "(4) the four-column CANNOT-PRESERVE table containing only rows you judge genuinely unpreservable, each with your reasoning; " +
    "(5) the refuted list; (6) coverage gaps auditors admitted; (7) the remediation plan: ordered work packages in dependency order — the event protocol and data model first, then whatever other packages depend on, grouping by module so each package is one coherent implementer session; each with scope, files, acceptance criteria, estimate. " +
    "Then return the structured plan. Keep every package self-contained: an implementer will receive only the package text plus the goal.",
  {
    label: "plan",
    phase: "Plan",
    model: MODEL,
    mode: "agent",
    cwd: REPO,
    configOptions: XHIGH,
    schema: PLAN_SCHEMA,
    retries: 1,
  }
);

if (!plan) {
  log("✘ synthesizer returned nothing; findings are in the run result");
  return { pin: PIN, previousPin: PREVIOUS_PIN, confirmed, refuted, candidates, plan: null };
}
log("plan: " + plan.packages.length + " packages → " + plan.reportPath);
return {
  pin: PIN,
  previousPin: PREVIOUS_PIN,
  reportPath: plan.reportPath,
  summary: plan.summary,
  counts: { raw: raw.length, confirmed: confirmed.length, refuted: refuted.length },
  packages: plan.packages,
  confirmed: confirmed.map(({ votes, ...f }) => f),
};
