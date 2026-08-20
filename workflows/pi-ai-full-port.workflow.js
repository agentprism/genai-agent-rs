export const meta = {
  name: "pi-ai-full-port",
  description:
    "Port the remainder of pi-ai (everything in scope: utils, models/provider framework + catalog, anthropic, google, bedrock, auth/OAuth, cloudflare/radius providers, compat surface, cli) into the agentprism-ai crate, plus the R6 epsilon fixes. codex/gpt-5.6-sol (xhigh) implements; claude/opus[1m] (xhigh) reviews; sequential gated phases, commit per approved phase.",
  phases: [
    { title: "Setup" },
    { title: "Q1 · epsilon fixes + utils + store/catalog primitives" },
    { title: "Q2 · models framework + catalog + providers + compat + cli" },
    { title: "Q3 · anthropic-messages" },
    { title: "Q4 · google (shared, generative-ai, vertex)" },
    { title: "Q5 · bedrock-converse-stream" },
    { title: "Q6 · auth + OAuth flows" },
    { title: "Q7 · cloudflare, radius, all.ts, legacy aliases" },
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
  "- The reference is pi-ai at " + PI + " (repo pinned at commit " + PIN + "). The crate is a FULL port of pi-ai " +
  "into idiomatic Rust: every in-scope feature and every observable behavior preserved, semantics preserved where " +
  "possible. Faithful means a consumer of the Rust `ai` crate observes no feature or behavior difference from pi-ai " +
  "beyond language semantics: same request bytes/headers and field-presence semantics, same event sequences and " +
  "ordering, same error strings, same usage/cost math, same defaults/fallbacks/retry behavior, same failure ORDERING " +
  "(which error wins when several conditions hold), same persisted/session JSON bytes.\n" +
  "- NEVER silently deviate or descope. If a behavior or feature genuinely cannot be preserved in Rust, implement " +
  "the closest faithful behavior AND report it in your phase report as a table row with EXACTLY these four columns: " +
  "Delta | Technical delta | pi counterpart (file:line) | Why it cannot be preserved. The reviewer judges every row " +
  "independently; a row that is actually achievable in Rust is a rejection. Platform/language-forced examples already " +
  "accepted: JS stack traces, V8 parser message text, lone UTF-16 surrogates inside a Rust String, Windows (descoped " +
  "by the owner), JS runtime-identity headers (X-Stainless-*), JS string/number coercion garbage.\n" +
  "- EXCLUDED BY OWNER RULING (do not port): api/lazy.ts and every *.lazy.ts (lazy module loading — factories are " +
  "plain eager constructors with the same names/semantics); auth/oauth/load.ts deferred-import mechanics and " +
  "bun-oauth.ts (the flow REGISTRY by provider id IS preserved); azure-openai-responses; mistral-conversations; " +
  "pi-messages; image generation (openrouter-images, images*.ts, image-models*, images-api-registry, " +
  "providers/images/*, providers/openrouter-images.ts); packages/agent's proxy protocol. Everything else in " + PI + "/src is in scope.\n" +
  "- DEPENDENCY SOURCING: provider-native SDK where one exists in native Rust; a community equivalent otherwise; " +
  "re-implement pi's logic when neither can match. A library is acceptable for TRANSPORT only if, verified in its " +
  "vendored source (~/.cargo/registry/src/index.crates.io-*/), it can produce pi's exact observable behavior: request " +
  "bytes and headers (single auth header, caller headers, null-suppression), real response status/headers to hooks, " +
  "raw error bodies for pi's error strings, header-directed retry, byte-correct streaming decode. If it cannot, use it " +
  "for TYPES only and own the transport — the crate's precedents are ai/src/api/openai_sse.rs and " +
  "ai/src/api/openai_codex_responses/transport.rs. Record the verification in the report. Dependencies are not " +
  "restricted; faithfulness decides. If `cargo fetch` is sandbox-denied for a crate you need, land the code and " +
  "report the exact crate@version — do not substitute a hand-rolled shim for a native SDK the ruling prefers.\n" +
  "- Read first: " + REPO + "/docs/porting-pi-ai-and-agent-core-docs/v2/preserved-architectural-seams-pi-ai-v2.mdx " +
  "(binding seam rulings), provider-api-implementations.mdx (per-module transport truths), and " + AUDIT + " " +
  "(defect classes already found and fixed — do not reintroduce them: oxide-style metadata loss, UTF-16 vs code-point " +
  "string ops, omitted-vs-null presence, strict decoding where pi duck-types, early abort checks that reorder failures, " +
  "hand-rolled parsers where a faithful helper exists).\n" +
  "- Layout mirrors pi file-for-file: src/models.rs <= models.ts; src/providers/<name>.rs <= providers/<name>.ts; " +
  "src/auth/... <= auth/...; src/utils/<name>.rs <= utils/<name>.ts; src/api/<name>.rs <= api/<name>.ts; " +
  "src/bin/pi-ai.rs <= cli.ts. Public re-exports in lib.rs mirror index.ts (plus the compat surface as a module).\n" +
  "- Work ONLY inside " + REPO + "/ai (plus ai/Cargo.toml and workspace Cargo.lock). NEVER modify anything under " +
  PI + " or " + REPO + "/genai. Never run cargo fmt on the genai/ subtree.\n" +
  "- Serde wire fidelity: the JSON these types serialize to IS pi's wire/session format — camelCase where pi uses " +
  "it, same discriminants, omitted-vs-present-vs-null preserved, insertion key order preserved where pi's objects " +
  "carry it onto the wire or into session files (serde_json preserve_order is on; use order-preserving maps).\n" +
  "- Comments: only constraints the code cannot show, plus pi file.ts:line provenance anchors. No narration.\n" +
  "- TESTS are part of the port: enumerate " + PI + "/test/ for your phase's subjects and port every applicable " +
  "test case-for-case, hermetically (local HTTP/WS servers; no network, no live keys). Live/e2e/smoke tests are " +
  "listed as skipped with a one-line reason. Where pi has no unit test for an observable behavior you port, pin it " +
  "with a test citing the pi source line. Catalog-driven tests use the embedded catalog once Q2 has landed.\n" +
  "- Gates you must run and make pass: cargo fmt -p agentprism-ai; cargo clippy -p agentprism-ai --all-targets -- " +
  "-D warnings; cargo test -p agentprism-ai; cargo build --workspace; git diff --check. If a command is sandbox-" +
  "denied, land the code, report the exact denied command, let the reviewer run it — never claim a gate passed that " +
  "you did not see pass.\n" +
  "- Do NOT git commit; the workflow commits after review approval.\n";

const PHASES = [
  {
    key: "q1",
    title: "Q1 · epsilon fixes + utils + store/catalog primitives",
    commitMsg: "ai: Q1 — epsilon fidelity fixes; port remaining utils, env-api-keys, models-store, model-catalog",
    scope:
      "SCOPE A — EPSILON FIXES (section E of " + AUDIT + " enumerated these; the owner ruled they are fixable and must match pi):\n" +
      "- Compat structural read: completions/responses must read model.compat structurally like pi (openai-completions.ts:276 and the compat getters) — a Model whose compat enum variant is not this module's family is NOT rejected; its fields are read as pi would (matching keys visible, others undefined → defaults). Remove the compat-variant terminal error.\n" +
      "- content:null preserved: a message with content:null must deserialize→reserialize byte-identically (session files are pi's persisted format); the []-coercion happens only where pi does it, in transformMessages (transform-messages.ts:73). Keep the A4 acceptance of null/missing.\n" +
      "- utils/estimate.ts becomes a public ai::utils::estimate module with pi's full export set (estimateTextTokens, estimateTextAndImageContentTokens, estimateMessageTokens, estimateContextTokens, calculateContextTokens, ContextUsageEstimate); simple_options consumes it. Port test/context-estimate.test.ts and test/tokens.test.ts / total-tokens.test.ts where they apply.\n" +
      "- Insertion order: ProviderHeaders / ProviderEnv and every map pi serializes (headers onto the wire in pi's order, metadata, samplingParams, compat/routing unknown keys) must preserve JS insertion order on serialize/reserialize — including interleaved known/unknown keys on compat and routing structs (replace flatten-hoisting with order-preserving serialization).\n" +
      "- event-stream result(): when a stream closes without a terminal event, result() stays pending exactly like pi's never-settling promise (event-stream.ts:16-17,64); the seam-#12 rule (producers always emit a terminal) is enforced by tests on producers, not by an error from result(). A panic inside a provider's spawned task must become a terminal error event carrying the panic message, as pi's catch turns any throw into an error event.\n" +
      "- JS number semantics for timeoutMs/maxRetries/maxRetryDelayMs/thinking budgets/usage counts: accept every JSON number pi accepts and behave as pi does with it (e.g. setTimeout semantics for negative/fractional/Infinity delays — Infinity coerces to 0ms, provider-retry.ts:86). Retry-After parsing: reproduce JS Number() and Date.parse() acceptance (hex/octal/binary/leading sign/whitespace/Infinity; ISO-8601 variants, RFC 2822/1123/850, asctime, V8's legacy fallbacks) for provider-retry.ts:61 and openai-codex-responses.ts:132-157.\n" +
      "- JS String() semantics where pi stringifies values: numeric metadata.raw (openai-completions.ts:679-680, exponent form 1e+21), truthy non-string phase in textSignature (openai-responses-shared.ts:49-53), truthy non-string JWT chatgpt_account_id (openai-codex-responses.ts:1585-1586).\n" +
      "Each fix carries a regression test citing pi file:line; tests that pinned the old divergence are corrected.\n\n" +
      "SCOPE B — PORT these pi sources (all of " + PI + "/src/utils not yet in ai/src/utils, plus root helpers):\n" +
      "utils/abort.ts, utils/abort-signals.ts, utils/sleep.ts, utils/text.ts (contentText), utils/uuid.ts (uuidv7 — byte layout and monotonicity as pi), utils/deferred-tools.ts, utils/diagnostics.ts (complete: formatThrownValue, extractDiagnosticError, append helpers), utils/retry.ts (agent-tier retry classifier tables and helpers), utils/overflow.ts (context-overflow classification), utils/validation.ts + utils/typebox-helpers.ts (tool-call argument validation against the tool's JSON schema with TypeBox's validation semantics AND its error message text for every error kind reachable from pi's schema subset — read typebox/error in ~/pi/node_modules; StringEnum as a runtime schema helper), utils/node-http-proxy.ts (outbound proxy selection from env/ProviderEnv incl. no_proxy rules and default ports — wire it so the crate's HTTP paths honor it as pi's fetch does), env-api-keys.ts, models-store.ts (ModelsStore trait, InMemoryModelsStore with structuredClone isolation and abort semantics), model-catalog.ts (flattenModelCatalog).\n" +
      "TESTS: abort.test.ts, text.test.ts, uuid.test.ts, deferred-tools.test.ts, retry.test.ts, overflow.test.ts, context-overflow.test.ts, validation.test.ts, node-http-proxy.test.ts, env-api-keys.test.ts, context-estimate.test.ts, lax-message-content.test.ts, provider-error-body-passthrough.test.ts, provider-error-body-regression.test.ts, error-body.test.ts (confirm already covered), tokens/total-tokens where applicable.",
  },
  {
    key: "q2",
    title: "Q2 · models framework + catalog + providers + compat + cli",
    commitMsg: "ai: Q2 — port models.ts (Models/Provider/createProvider/createModels), embedded catalog, provider factories, faux, compat surface, cli",
    scope:
      "SCOPE — pi-ai's core orchestration, in full:\n" +
      "- models.ts (944 lines) → src/models.rs: Provider (id, name, baseUrl, auth, models, api, headers/env hooks, everything in the interface at :97-155), Models/MutableModels (:156-231) and createModels (:735) — registration/replacement/removal, getModel/getModels/getProviders and every query, stream/streamSimple/fetchDeferred/cancelDeferred dispatch with the seam-#2 in-band error for a model whose api has no implementation, seam-#4 applyAuth layering with transformHeaders last, ModelsRequestTransforms, ModelsPublication/refresh (refreshModels with ETag/If-None-Match + Last-Modified against a ModelsStore, RefreshModelsContext/Options/Result exactly as :39-77), ModelsError (from auth/resolve.ts), createProvider (:762) with its validation and defaults, hasApi, calculateCost, getSupportedThinkingLevels, clampThinkingLevel, modelsAreEqual (:874-944). The existing ai/src/models.rs helpers are absorbed.\n" +
      "- auth core needed by models/providers (OAuth flows are Q6): auth/types.ts, auth/context.ts, auth/credential-store.ts (pluggable store trait — the owner's standing requirement: a host such as an iOS/macOS app supplies its own store; no shell-outs), auth/helpers.ts (envApiKeyAuth etc.), auth/resolve.ts.\n" +
      "- Catalog: models.generated.ts + providers/*.models.ts + providers/data/*.json + data/.manifest.json → embedded verbatim at the pin (include_str!/build-time), exposed with pi's names (e.g. OPENAI_MODELS) as typed Model values; the manifest is honored as pi's all.ts does.\n" +
      "- Provider factories for every API the crate implements after this phase (all openai-family providers under providers/*.ts: openai, openai-codex, openrouter, deepseek, groq, cerebras, together, fireworks, xai, zai, zai-coding-cn, moonshotai(-cn), minimax(-cn), nvidia, huggingface, baseten, opencode, opencode-go, qwen-token-plan(-cn/-individual), xiaomi(-token-plan-*), ant-ling, kimi-coding, vercel-ai-gateway, github-copilot's openai half), providers/all.ts with builtinProviders()/KnownProvider for those (later phases append), providers/faux.ts in full (test/demo provider exported from index).\n" +
      "- compat.ts surface (the old global API) minus lazy/images per the exclusions: api-dispatch stream()/complete() with env API key injection, the api registry, getModel/getModels/getProviders over the generated catalog; legacy-api-aliases.ts for the ported APIs (deprecated streamX wrappers, marked #[deprecated] with pi's messages).\n" +
      "- cli.ts → src/bin/pi-ai.rs: `login [provider]`, `list`, `help` with pi's exact prompts/output, auth.json read/write semantics (JSON pretty 2-space), OAuth login via the Q6 flows (stub the flow registry call so the bin compiles now and Q6 fills it).\n" +
      "TESTS: models-runtime.test.ts, providers.test.ts, stream.test.ts, faux-provider.test.ts, compat-env.test.ts, model-catalog-types.test.ts, model-data-validation.test.ts, generate-models-strict.test.ts (data-side assertions), the catalog tests baseten-/fireworks-/together-/xiaomi-/zai-coding-plan-/qwen-token-plan-/openrouter-cache-control-models.test.ts, cache-retention.test.ts, reasoning-options.test.ts, sampling-options.test.ts, telemetry-options.test.ts, fetch-option.test.ts, responseid.test.ts, cross-provider-handoff.test.ts, tool-call-without-result.test.ts, empty.test.ts, zen.test.ts; tests whose subject is anthropic/google/bedrock are listed for Q3–Q5.",
  },
  {
    key: "q3",
    title: "Q3 · anthropic-messages",
    commitMsg: "ai: Q3 — port anthropic-messages (+ anthropic, github-copilot anthropic half, xiaomi-ams providers)",
    scope:
      "SCOPE — " + PI + "/src/api/anthropic-messages.ts (1391 lines) → ai/src/api/anthropic_messages.rs, implementing ProviderStreams, and the provider factories that depend on it (providers/anthropic.ts, github-copilot.ts anthropic half, xiaomi-token-plan-ams etc. — check each providers/*.ts api import). pi uses @anthropic-ai/sdk for the REQUEST (client.messages.create with betas, default headers, auth-token vs api-key) and hand-parses the SSE itself. Transport truth and betas composition are in provider-api-implementations.mdx; read the pi file in full first.\n" +
      "Contract (byte/behavior): request body field-for-field incl. thinking/adaptive thinking and effort, tool definitions and tool_choice, cache_control breakpoints, system prompt placement, betas header composition (caller betas + feature-implied, in pi's order), auth header selection (x-api-key vs Authorization: Bearer for OAuth tokens) and null-suppression; SSE event handling for every event type pi handles incl. redacted thinking, signature deltas, citations, server tool use, ping, error events, overloaded (529) handling as pi does (NOT collapsed to rate-limit); usage/cost math incl. cache write 1h; stop-reason mapping + rawStopReason; error strings via normalizeProviderError/formatProviderError; retry via provider_retry with real headers; onPayload/onResponse with real data; tool-name normalization; cross-model thinking replay rules.\n" +
      "Dependency: no official Anthropic Rust SDK exists. Community candidate adk-anthropic (zavora-ai/adk-rust) — known gaps vs pi: caller betas on Messages (PR #594 pending), fallbacks (#595), 529→rate_limit collapse in its client, no raw-body hook. Verify in vendored source; given pi hand-parses SSE anyway, expect to use a library for request/response TYPES at most and own the transport (openai_sse.rs precedent). Record the decision and verification.\n" +
      "TESTS: every anthropic-*.test.ts except *-e2e/*-smoke (listed skipped with reason), github-copilot-anthropic.test.ts, transform-messages-copilot-openai-to-anthropic.test.ts, interleaved-thinking.test.ts, max-thinking.test.ts, xhigh.test.ts, supports-xhigh.test.ts, tool-call-id-normalization.test.ts (anthropic cases), anthropic-auth-token.test.ts, image-tool-result.test.ts (anthropic cases).",
  },
  {
    key: "q4",
    title: "Q4 · google (shared, generative-ai, vertex)",
    commitMsg: "ai: Q4 — port google-shared, google-generative-ai, google-vertex (+ google, google-vertex providers)",
    scope:
      "SCOPE — " + PI + "/src/api/google-shared.ts (452), google-generative-ai.ts (526), google-vertex.ts (598) → ai/src/api/google_shared.rs, google_generative_ai.rs, google_vertex.rs, plus providers/google.ts and providers/google-vertex.ts. pi uses @google/genai END-TO-END (request + streaming parse) for both, with Vertex configured via project/location and ADC/service-account/API-key resolution (google-vertex-api-key-resolution.test.ts pins the precedence).\n" +
      "Contract: request bodies byte-identical (contents/parts lowering incl. tool results and images routing, systemInstruction, generationConfig incl. thinkingConfig with thinkingLevel/thinkingBudget/includeThoughts and the thinking-level map, tools/function declarations incl. the StringEnum-compatible schemas, toolConfig), thought signatures and Gemini-3 unsigned tool-call handling, signed empty blocks, streaming event sequences, usage/cost, raw stop reasons, retry rules (google-shared-retry.test.ts), error strings, onPayload/onResponse with real data, x-goog-api-key vs OAuth bearer for Vertex, endpoint/URL construction for both APIs.\n" +
      "Dependency: no official Google GenAI Rust SDK. Community candidate adk-gemini (zavora-ai/adk-rust: thoughtSignature, thinkingLevel/budget/includeThoughts, Vertex feature with google-cloud-auth for ADC/SA/WIF). Verify in vendored source against the contract; own the transport where it cannot match. For Vertex credentials, a native Google auth crate (google-cloud-auth or equivalent) is acceptable for token acquisition if its precedence can be made to match pi's resolution order; otherwise implement pi's resolution.\n" +
      "TESTS: every google-*.test.ts and google-shared-*.test.ts, google-vertex-api-key-resolution.test.ts, image-tool-result.test.ts (google cases), tool-call-id-normalization.test.ts (google cases).",
  },
  {
    key: "q5",
    title: "Q5 · bedrock-converse-stream",
    commitMsg: "ai: Q5 — port bedrock-converse-stream (+ amazon-bedrock provider, bedrock-provider module)",
    scope:
      "SCOPE — " + PI + "/src/api/bedrock-converse-stream.ts (1325) → ai/src/api/bedrock_converse_stream.rs, bedrock-provider.ts → the module re-export, providers/amazon-bedrock.ts. pi uses the AWS SDK (ConverseStream) — the wire is AWS event-stream framing, not SSE. The owner's ruling for Bedrock is BEHAVIORAL parity: use the official native Rust SDK (aws-sdk-bedrockruntime + aws-config) for transport and framing; every behavior pi layers on top must match exactly.\n" +
      "Contract: message/tool conversion (bedrock-convert-messages.test.ts), credential resolution order and explicit credentials/profile/region handling (bedrock-credentials.test.ts), endpoint resolution (bedrock-endpoint-resolution.test.ts), custom headers (bedrock-custom-headers.test.ts), thinking payloads and redacted reasoning (bedrock-thinking-payload/redacted-reasoning), raw stop reasons, error metadata surfacing (bedrock-error-metadata.test.ts — $metadata.httpStatusCode / $response precedence in error-body), response headers to onResponse (bedrock-response-headers.test.ts), usage/cost, retry behavior as pi configures the SDK (maxAttempts etc.), onPayload.\n" +
      "TESTS: every bedrock-*.test.ts; bedrock-utils.ts is shared test scaffolding to port.",
  },
  {
    key: "q6",
    title: "Q6 · auth + OAuth flows",
    commitMsg: "ai: Q6 — port OAuth flows (anthropic, openai-codex, github-copilot, openrouter, kimi-coding, xai, radius, device-code, pkce, oauth-page) and the flow registry",
    scope:
      "SCOPE — " + PI + "/src/auth/oauth/*.ts → ai/src/auth/oauth/*.rs: anthropic.ts, openai-codex.ts, github-copilot.ts, openrouter.ts, kimi-coding.ts, xai.ts, radius.ts (+ providers/radius-config.ts), device-code.ts, pkce.ts, oauth-page.ts (the callback HTML page, byte-identical), load.ts's flow REGISTRY by provider id (registerBundledOAuthFlowLoaders semantics minus the deferred-import mechanics — excluded by ruling; report that row), compat/extension-oauth-types.ts and oauth.ts (type re-exports). Wire the Q2 cli bin's login to the registry.\n" +
      "Contract: every flow's exact HTTP exchanges (authorize URLs and parameters, PKCE S256 derivation, token/refresh/device-code endpoints and bodies, headers incl. user-agent/originator, polling intervals and slow_down/authorization_pending handling, expiry math and refresh thresholds, error messages), the OAuthLoginCallbacks/prompt/notify event sequence (auth_url, device_code, info, progress), local callback server behavior where pi runs one (port, path, success/failure page bytes), credential shapes persisted (OAuthCredential fields byte-identical for auth.json/credential-store compatibility), the AuthContext/resolve integration (token refresh on use, ModelsError codes).\n" +
      "TESTS: oauth-auth.test.ts, oauth-device-code.test.ts, anthropic-oauth.test.ts, openai-codex-oauth.test.ts, github-copilot-oauth.test.ts, openrouter-oauth.test.ts, kimi-coding-oauth.test.ts, xai-oauth.test.ts, radius-oauth.test.ts; test/oauth.ts scaffolding.",
  },
  {
    key: "q7",
    title: "Q7 · cloudflare, radius, all.ts, legacy aliases",
    commitMsg: "ai: Q7 — port cloudflare gateway binding + providers, radius provider; complete providers/all.ts and legacy-api-aliases",
    scope:
      "SCOPE — api/cloudflare-gateway-binding.ts (192: a FetchFunction shim translating gateway-prefixed HTTPS requests into calls on a caller-supplied, structurally-typed Workers AI binding — port the structural trait exactly as pi defines AiGatewayBinding/AiGatewayBindingGateway/AiGatewayUniversalRequestLike, the URL→{provider,endpoint,headers,query} translation, the auth sentinel and header stripping, and the rejection rules for out-of-prefix/non-POST/non-JSON requests), providers/cloudflare-auth.ts, cloudflare-stream.ts, cloudflare-ai-gateway.ts, cloudflare-workers-ai.ts, providers/radius.ts (+ radius-config from Q6), and completion of providers/all.ts (builtinProviders, KnownProvider, manifest handling for every in-scope provider), legacy-api-aliases.ts for every in-scope API, the cli provider list.\n" +
      "FINAL SWEEP (this phase also gates the whole port): diff " + PI + "/src against ai/src file-for-file and list every pi export (index.ts, compat.ts, providers/all.ts, api/*.ts public functions) with its Rust counterpart or its exclusion ruling; anything missing is a rejection. Run the full test suite and confirm every pi test file is mapped (ported / skipped-with-reason / excluded-by-ruling).\n" +
      "TESTS: cloudflare-gateway-binding.test.ts, cloudflare-stream.test.ts (+ cloudflare-utils.ts scaffolding), providers.test.ts (full), lazy-module-load.test.ts (excluded — list it).",
  },
];

function implPrompt(ph, feedback, attempt) {
  return (
    "You are the IMPLEMENTER (" + IMPLEMENTER + ", xhigh reasoning) working in " + REPO + ".\n\n" +
    COMMON + "\n" + ph.scope + "\n\n" +
    (feedback
      ? "A REVIEWER REJECTED your previous attempt (round " + attempt + "). Your prior edits are still on disk; fix " +
        "every point below without regressing what was already correct:\n" + feedback + "\n\n"
      : "") +
    "Return a concise plaintext report: files added/changed; dependency decisions with the vendored-source " +
    "verification; the pi-test → Rust-test mapping (ported / pinned-from-source / skipped-with-reason); gate commands " +
    "run with results; any sandbox-denied commands or crates; and the CANNOT-PRESERVE table (four columns as " +
    "specified) — empty if nothing was left behind."
  );
}

function reviewPrompt(ph, implReport) {
  return (
    "You are the REVIEWER (" + REVIEWER + ", xhigh effort) in " + REPO + ".\n" +
    "STRICT RULE: do NOT modify, create, stage, or commit any file. Read and run verification commands only.\n\n" +
    "The phase under review:\n" + ph.scope + "\n\n" + COMMON + "\n" +
    "The implementer reported:\n" + (implReport == null ? "(no report)" : String(implReport)) + "\n\n" +
    "Judge the ACTUAL working-tree changes (git status + git diff + reading the files) against pi at " + PI + " with " +
    "the rigor of an adversarial faithfulness audit:\n" +
    "1. COMPLETENESS: every pi source file and export in this phase's scope has a Rust counterpart; nothing descoped " +
    "or deferred. A 'later'/'TODO'/'follow-up' is a rejection.\n" +
    "2. BEHAVIOR: compare each ported function against pi line-by-line for observable behavior — request bytes and " +
    "headers, presence semantics, event ordering, error strings, usage math, defaults, failure ordering, retry. " +
    "Spot-verify at least 20 substantive behaviors against pi file:line and list each.\n" +
    "3. CANNOT-PRESERVE TABLE: judge every row independently. If Rust can achieve the behavior (including via an " +
    "order-preserving map, a custom serializer, a finite message table, an own transport), the row is a rejection.\n" +
    "4. DEPENDENCIES: confirm the vendored-source verification claims (open the crate source yourself) — a library " +
    "that cannot match pi's bytes/metadata must not be on the transport path.\n" +
    "5. SEAMS: streams never return Result; failures in-band; partial-free protocol + MessageBuilder; open unions; " +
    "two-tier options; headers sentinel; pluggable credential store with no shell-outs.\n" +
    "6. TESTS: pi-test mapping complete for the scope; source-derived pins cite pi lines; all hermetic.\n" +
    "7. GATES: independently run cargo fmt -p agentprism-ai -- --check, cargo clippy -p agentprism-ai --all-targets " +
    "-- -D warnings, cargo test -p agentprism-ai, cargo build --workspace, git diff --check; confirm only ai/ (+ " +
    "Cargo.toml/lock) changed.\n" +
    "Set ok=true ONLY if the phase is genuinely complete and faithful with no blocking defect. Otherwise ok=false with " +
    "complete, file-and-line-specific feedback — it is the implementer's ONLY context next round."
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
      description: "Rows the reviewer ACCEPTS as genuinely unpreservable (each independently judged).",
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
log("pi-ai full port · impl=" + IMPLEMENTER + "/xhigh · rev=" + REVIEWER + "/xhigh · ≤" + MAX_ROUNDS + " rounds/phase · pi pin " + PIN.slice(0, 9));

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
      "Then read back the SHA with git rev-parse HEAD. If git status shows nothing to commit, make no commit. Do NOT " +
      "modify any file, amend history, push, or touch other branches.",
    { label: "commit:" + ph.key, phase: ph.title, model: COMMITTER, mode: "bypassPermissions", cwd: REPO, schema: COMMIT_SCHEMA }
  );
  const c = commit || { committed: false, sha: "", note: "commit agent returned null" };
  const shaOk = typeof c.sha === "string" && /^[0-9a-f]{7,40}$/i.test(c.sha.trim());
  const sha = c.committed === true && shaOk ? c.sha.trim() : null;
  log("✔ " + ph.key + " approved after " + outcome.attempts + " round(s)" + (sha ? " · committed " + sha.slice(0, 9) : " · ⚠ no commit: " + c.note) +
      (Array.isArray(verdict.cannotPreserve) && verdict.cannotPreserve.length ? " · cannot-preserve rows: " + verdict.cannotPreserve.length : ""));
  results.push({ phase: ph.key, approved: true, rounds: outcome.attempts, sha: sha, summary: verdict.summary || "", cannotPreserve: verdict.cannotPreserve || [] });
}

return { pin: PIN, results: results };
