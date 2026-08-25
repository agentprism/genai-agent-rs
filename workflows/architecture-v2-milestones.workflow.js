export const meta = {
  name: "architecture-v2-milestones",
  description:
    "Build the pi Rust port milestone by milestone, exactly as specified in docs/porting-pi-ai-and-agent-core-docs/architecture-v2-part1-proposal.md and architecture-v2-part2-revision.md under goal.md. One run = one milestone (args.milestone). Per package: a codex/gpt-5.6-sol xhigh implementer and an independent codex/gpt-5.6-sol xhigh reviewer gate up to N rounds, then a commit. No Claude agents.",
  model: "codex/gpt-5.6-sol",
  phases: [{ title: "Preflight" }, { title: "Build" }, { title: "Closeout" }],
};

const A = typeof args === "string" ? JSON.parse(args) : args || {};
const REPO = A.repo || "/home/vikash/genai-agent/genai-agent-rs";
const PI_ROOT = A.piRoot || "/home/vikash/pi-pin-8fa7eebd2";
const PI_AI = PI_ROOT + "/packages/ai";
const PI_AGENT = PI_ROOT + "/packages/agent";
const PIN = A.pin || "8fa7eebd235355522c8104166b4f1f959b4e2f10";
const BRANCH = A.branch || "main";
const MILESTONE = A.milestone || "M1";
const ONLY = Array.isArray(A.packages) ? A.packages : null;
const MAX_ROUNDS = A.maxRounds || 6;
// Resumption of a halted package: allowDirty lets preflight accept uncommitted work inside the allowed paths
// (the implementer treats it as material), initialFeedback seeds round 1 with the last rejection, and
// priorApproved lists packages already committed by an earlier run so the closeout record is complete.
const ALLOW_DIRTY = A.allowDirty === true;
const INITIAL_FEEDBACK = typeof A.initialFeedback === "string" && A.initialFeedback.trim() ? A.initialFeedback : null;
const PRIOR_APPROVED = Array.isArray(A.priorApproved) ? A.priorApproved : [];
const DOCS = REPO + "/docs/porting-pi-ai-and-agent-core-docs";
const GOAL = DOCS + "/goal.md";
const ARCH1 = DOCS + "/architecture-v2-part1-proposal.md";
const ARCH2 = DOCS + "/architecture-v2-part2-revision.md";
const MODEL = "codex/gpt-5.6-sol";
const XHIGH = { reasoning_effort: "xhigh" };

const COMMON =
  "GOVERNING DOCUMENTS — read these in full before anything else, in this order: " + GOAL + " (the standard), " +
  ARCH1 + " (architecture part 1), " + ARCH2 + " (architecture part 2; takes precedence over part 1 where they " +
  "differ). The architecture was adopted by the owner as written. It is the authority for SHAPE: crate layout, " +
  "type and trait definitions, protocol, policies, error model, runtime model. Implement what it says; do not " +
  "redesign it, simplify it, or merge its crates. Where it sketches a type or trait, that sketch is the " +
  "specification (names, fields, variants, bounds); where it leaves a detail open, choose the idiomatic Rust that " +
  "keeps every stated property and say what you chose.\n\n" +
  "BEHAVIOR AUTHORITY: pi source at " + PI_ROOT + " (packages/ai, packages/agent), pinned at " + PIN + ", is the " +
  "reference implementation for every behavior the architecture maps to pi. Read the pi files the relevant " +
  "sections cite, in full. Where an architecture document and pi source disagree about a behavior that is not on " +
  "the divergence allowlist (part 2 §10.11), pi is right: implement pi's behavior and add a correction note to the " +
  "document section (a short blockquote starting 'Correction:'). Do not introduce any other divergence from pi.\n\n" +
  "WHAT DONE MEANS (goal.md, part 2 §10): the parity manifest at " + REPO + "/parity/manifest.toml maps every " +
  "upstream packages/ai/test/**/*.test.ts and packages/agent/test/**/*.test.ts file to named Rust tests with status " +
  "semantic-parity or deliberate-divergence (with reason), or planned (with the milestone that will port it); the " +
  "named conformance suites in part 2 §10.1–§10.10 define correct behavior and every test you write that realizes " +
  "one of them uses that exact name and cites its pi basis; provider request bodies are byte-identical to pi's " +
  "where §10.8 applies. Update the manifest for every test you add or port. Tests are hermetic: no network, no " +
  "live keys, ScriptedRuntime and captured fixtures only.\n\n" +
  "IDIOM: idiomatic Rust — ownership, Result at boundaries (part 1 §3.3 error semantics as revised by part 2 §2), " +
  "object-safe traits with the BoxFuture/BoxStream aliases the architecture names, both the Send and Local trait " +
  "families (part 2 §9.2), the portable CancellationToken (part 2 §9.5), no Tokio in the core crates, no " +
  "process spawning, no impersonation of any runtime or SDK, no f64 money, redacted secret types. Public types " +
  "that persist carry schema_version. Keep scratch state out of persisted schemas (replay invariant R8).\n\n" +
  "CONSTRAINTS: work only inside " + REPO + "/crates, /providers, /bindings, /examples, /parity, the workspace " +
  "Cargo.toml and Cargo.lock, and " + DOCS + " (correction notes only). Never modify " + PI_ROOT + ". The legacy " +
  "crates on the legacy/pre-architecture-v2 branch (ai/, genai/, agent/, ffi/) are a quarry — `git show " +
  "legacy/pre-architecture-v2:ai/src/<path>` — for wire encoders, SSE decoders, OAuth flows, and test ideas; " +
  "they are not a baseline and their shapes must not leak in. Gates: cargo fmt --all -- --check; cargo clippy " +
  "--workspace --all-targets -- -D warnings; cargo build --workspace; cargo test --workspace; cargo doc " +
  "--workspace --no-deps; bash parity/check.sh (once it exists); git diff --check. Never claim a gate you did " +
  "not see pass; if the sandbox denies a command or a crate fetch, say exactly which. Do not git commit; the " +
  "workflow commits after approval.\n";

// Packages quote the architecture documents' own deliverable lists. `read` = sections to read in full;
// `deliver` = what must exist when the package is done; `accept` = the conformance tests / checks the reviewer runs.
const MILESTONES = {
  M1: {
    title: "Milestone 1 — contracts and fake runtime (part 1 §10 M1; part 2 §1, §2.1, §3.3, §5.1–5.2, §9)",
    packages: [
      {
        id: "M1.1",
        title: "pi-ai canonical data model, replay envelope, descriptors, usage/cost, finish/error types",
        crate: "crates/pi-ai",
        read: "part 1 §3.1 (canonical data model, messages, Conversation schema_version, opaque metadata — superseded by part 2 §1.2), §3.9 (usage and cost); part 2 §1.1–1.2 (AssistantMessage, ReplayEnvelope, ReplayScope, ReplayItem, ReplayTarget, ReplayApplicability, ReplayCompleteness, OpaquePayload, ContentBlock with stable ids), §1.4–1.8 'Persisted representation' blocks (the JSON shapes), §2.1 (AssistantFinish, AssistantFinishReason, PublicError), §5.1 (ModelDescriptor, CommonModelDescriptor, ApiModelConfig, typed compat structs, ThinkingLevelMap, LevelSupport, ExtensionMap, VersionedExtension), §5.2 (ModelPricing, TokenPriceRates, MoneyRate), §6.6 (OAuthCredential, ProviderOAuthExtra); pi types.ts in full.",
        deliver: [
          "Open string newtypes ProviderId, ModelId, ApiId, ReplayKind, ExtensionId; ModelRef; MessageId, ContentBlockId, ToolCallId, ReplayItemId, RunId, Timestamp (part 1 §3.1, part 2 §1.2).",
          "Message / UserMessage / AssistantMessage / ToolResultMessage / ContentBlock (Text, Image, Thinking{redacted}, ToolCall) with stable block ids; Conversation and Context with schema_version (part 1 §3.1; part 2 §1.2).",
          "ReplayEnvelope, ReplayScope, ReplayItem, ReplayTarget, ReplayApplicability, ReplayCompleteness, OpaquePayload with the JSON encodings shown in part 2 §1.4–1.8 (utf8 / bytes_base64 / json_bytes_base64) and helpers complete_item_for_block / items_for_block / complete_item / is_complete_and_applicable / as_utf8 / as_bytes / json_bytes (part 2 §1.2, §1.4–1.8).",
          "AssistantFinish, AssistantFinishReason {Stop, Length, ToolUse, Deferred, Error, Aborted}, PublicError (part 2 §2.1).",
          "ToolSpec, ToolCall, ToolResultContent; Usage (cumulative semantics, UsageSource) and Cost{currency, micros: u128}; ModelPricing / TokenPriceRates / RequestWidePriceTier / CacheWriteRetentionPricing with integer MoneyRate and integer cost arithmetic (part 1 §3.9; part 2 §5.2).",
          "ModelDescriptor { common, api: ApiModelConfig, extensions }, CommonModelDescriptor, ModalityCapabilities, ModelLimits, HeaderMapSpec, the typed ApiModelConfig enum with every variant in part 2 §5.1 including Custom, OpenAiCompletionsCompat / AnthropicMessagesCompat with every listed field, ThinkingLevelMap<T> / LevelSupport<T>, ExtensionMap / VersionedExtension and the four extension rules.",
          "SecretString (redacted Debug, no Serialize by default), OAuthCredential and ProviderOAuthExtra with Custom round-trip (part 2 §6.6).",
          "Versioned serde for every persisted type; schema_version fields; serialization round-trip tests for every type and for every persisted-representation JSON block in part 2 §1.4–1.8 (byte-for-byte where the document shows JSON).",
        ],
        accept: [
          "Every type named in the read sections exists with the documented fields/variants; no f64 money anywhere; secrets never Debug/Serialize in clear.",
          "Round-trip tests pass for all persisted types; the part 2 §1.4–1.8 persisted JSON examples parse and re-serialize to the shown shape.",
          "cargo doc --no-deps documents every public item with its architecture section.",
        ],
      },
      {
        id: "M1.2",
        title: "Streaming protocol, AssistantAssembler, AssistantStream, replay invariants",
        crate: "crates/pi-ai",
        read: "part 1 §3.3 (streaming protocol, assembler, tool-call arguments, error semantics); part 2 §1.3 (AssistantEvent exactly as listed, ReplayDataOperation, AssistantAssembler with apply/snapshot/finish_completed/finish_failed/finish_cancelled, strict completion validation, incomplete retention), §1.4–1.8 'Rust events' blocks, §1.9 (replay invariants R1–R8 and the primary proof fixture), §2.1 (exact failed and cancelled records), §9.2–9.3 (SendBoxStream/LocalBoxStream, AssistantStream is 'static); pi utils/event-stream.ts and the stream-handling parts of each api/*.ts the sections cite.",
        deliver: [
          "AssistantEvent with exactly the variants and fields of part 2 §1.3; ReplayDataOperation; ContentBlockKind.",
          "AssistantAssembler (IndexMap-backed builders) implementing apply / snapshot / finish_completed (validate_successful_replay) / finish_failed / finish_cancelled; failed and cancelled records built exactly as part 2 §2.1 specifies (same message id, content so far, complete items preserved, incomplete items marked, last known usage/response id/model, finish.error populated, code 'cancelled' for cancellation).",
          "AssistantStream (Send) and LocalAssistantStream over the boxed stream aliases; fused after terminal.",
          "Tests: replay invariants R1–R8 as named tests; the event sequences in part 2 §1.4–1.8 fed to the assembler produce the shown ContentBlock/ReplayItem values; the primary proof fixture shape (assemble → serialize → deserialize) for each family with the encoder step deferred to Milestone 4 and recorded as planned in the manifest.",
        ],
        accept: [
          "§10.1 tests realizable without a provider pass with these exact names: stream_start_precedes_content, stream_exactly_one_terminal, stream_no_event_after_terminal, stream_failure_is_terminal_message, stream_cancellation_is_terminal_message, stream_partial_identity_is_stable, stream_response_id_is_preserved, stream_response_model_is_preserved, stream_usage_is_cumulative, stream_tool_json_scratch_not_persisted, stream_binary_scratch_not_persisted.",
          "§10.2 assembly/round-trip tests pass for every family: *_fragments_append_in_order, *_survives_message_round_trip, *_failed_partial_*_is_not_replayed / *_incomplete_*_is_not_replayed, google_*_stays_on_* and google_empty_signed_*_is_retained; encoder-dependent names are present in the manifest as planned for M4.",
        ],
      },
      {
        id: "M1.3",
        title: "ModelRuntime (Send and Local), CancellationToken, ModelRequest and options, ScriptedRuntime",
        crate: "crates/pi-ai",
        read: "part 1 §3.2 (ModelRuntime, BoxFuture rationale), §3.4 (SimpleGenerationOptions — revised by part 2 §3.3: one API-specific patch via ApiOptionsInput, conflict error, precedence), §5 (the precise seam), §8 (ScriptedRuntime builder); part 2 §2.1 (RequestStartError), §9.1–9.5 (executor neutrality, Local and Send trait families, run stream lifetime, portable CancellationToken with child tokens), §9.6.",
        deliver: [
          "SendBoxFuture / LocalBoxFuture / SendBoxStream / LocalBoxStream aliases; ModelRuntime (Send + Sync + 'static) and LocalModelRuntime exactly as part 2 §9.2; ModelRequest; RequestStartError.",
          "CancellationToken per part 2 §9.5: cancel / is_cancelled / cancelled() future / child(); no tokio dependency; tests for child propagation and waking.",
          "SimpleGenerationOptions with the part 1 §3.4 fields; ApiOptionsInput<A> {None, Typed, Erased} and ErasedApiOptionsPatch with schemaVersion; LoweringError::ConflictingApiOptions; ReasoningLevel, ReasoningFallback {Strict, Clamp}, ThinkingBudgets, CacheRetention, ToolChoice, SamplingPlan types the lowering sections name (part 2 §3.3–3.4, §3.7).",
          "ScriptedRuntime with the part 1 §8 builder (text_response, tool_call_response, and additionally scripted replay items, failures, cancellations, usage) implementing both ModelRuntime and LocalModelRuntime; it must be able to produce every event sequence in part 2 §1.4–1.8.",
          "Tests: §10.1 names re-run against ScriptedRuntime; cancellation before/during a scripted stream yields the part 2 §2.1 cancelled record.",
        ],
        accept: [
          "A consumer can hold Arc<dyn ModelRuntime> with no knowledge of Models; the Local family compiles on a single-threaded target without Send bounds.",
          "ScriptedRuntime reproduces each part 2 §1.4–1.8 sequence and the assembler yields the documented values.",
        ],
      },
      {
        id: "M1.4",
        title: "Parity manifest, checker, and CI wiring",
        crate: "parity",
        read: "part 2 §10 (the manifest format and the five CI failure rules), §10.11 (divergence allowlist); goal.md 'What parity means'; the upstream test inventories at " + PI_AI + "/test and " + PI_AGENT + "/test.",
        deliver: [
          "parity/manifest.toml in the part 2 §10 format: upstream_repository, upstream_commit = the pin; one [[mapping]] for every packages/ai/test/**/*.test.ts and packages/agent/test/**/*.test.ts file (status semantic-parity with the Rust test names that exist today, deliberate-divergence with reason for files covered by §10.11, or planned with the milestone that will port them); one [[mapping]] per §10.11 allowlist row pointing at its replacement.",
          "parity/check.sh (and the Rust or Python it drives, kept dependency-free): fails when an upstream test file is absent from the manifest, a semantic-parity Rust test name does not exist in the workspace (discover via cargo test -- --list), a deliberate-divergence lacks a reason, or the pinned commit in the manifest differs from the checked-out pi source's HEAD when " + PI_ROOT + " is available; prints a coverage summary by status.",
          "The CI step in .github/workflows/ci.yml already calls parity/check.sh; make it pass.",
        ],
        accept: ["bash parity/check.sh passes; every upstream test file is present; counts by status are printed; removing a mapped test from the manifest makes it fail."],
      },
    ],
  },
  M2: {
    title: "Milestone 2 — agent loop against ScriptedRuntime (part 1 §10 M2, §4; part 2 §2.1–2.3, §4.4, §8, §9)",
    packages: [
      {
        id: "M2.1",
        title: "Agent state, records, events, outcomes, snapshot",
        crate: "crates/pi-agent-core",
        read: "part 1 §4 (all subsections), §7 (error hierarchy); part 2 §2.1 (RunOutcome revised: committed_message_id), §2.2, §4.4 (ContextPrepared event), §8.1 (state.streamingMessage / pendingToolCalls mapping); pi packages/agent/src/types.ts, agent.ts, agent-loop.ts, README.md in full.",
        deliver: [
          "AgentState{schema_version, system_prompt, model: ModelRef, reasoning, transcript: Vec<AgentRecord>}, AgentRecord {Llm, Custom{type_name, payload}}, AgentSnapshot{schema_version, state, next_sequence, streaming: Option<AssistantMessageSnapshot>, pending_tool_calls}, AgentEvent with the part 1 §4.4 variants plus ContextPrepared{turn, target, report}, AgentEventEnvelope{sequence, run_id, event}, RunOutcome per part 2 §2.1, TurnOutcome, AgentError (invariant/config only) per part 1 §7.",
          "Restore process per part 1 §4.9 (migrate schema → resolve ModelRef → bind ToolRegistry → validate custom kinds → construct).",
        ],
        accept: ["Snapshot round-trips; replaying committed events reproduces the final AgentState (part 1 §8 property test)."],
      },
      {
        id: "M2.2",
        title: "Run state machine, phases, queues, continue/retry/reset",
        crate: "crates/pi-agent-core",
        read: "part 2 §8.2 (AgentPhase and the critical order), §8.3 (event sequences), §8.4 (AgentControl, QueueCommand, acknowledgement), §2.2–2.3 (failed-turn projection, continue_run, retry_last_turn), §8.1 rows for steer/followUp/queue modes/continue/reset/waitForIdle/abort/subscribe; part 1 §4.3 (run(&mut self), backpressure); pi agent-loop.ts and agent.ts in full.",
        deliver: [
          "Agent::run(&mut self, input, cancellation) -> SendBoxStream<'a, AgentEvent> and the Local equivalent; AgentPhase enum; the exact phase order; AgentControl{steer, follow_up, cancel} with QueueCommand sequence numbers and QueueDrainMode {One, All} per queue; continue_run with pi's assistant-tail precondition and queue drain; retry_last_turn; reset_transcript (idle-only, part 2 §8.1 retention semantics) and reset_all; prompt_text / prompt_records; AgentInput.",
          "Failed and cancelled runs commit the part 2 §2.1 record before TurnFinished/RunFinished; provider projection omits Error/Aborted assistants via ContextPolicy (part 2 §2.2).",
        ],
        accept: [
          "§10.9 Lifecycle, Failure and cancellation, Queues, and State management tests pass with their exact names against ScriptedRuntime; event sequences match part 2 §8.3 verbatim; queue_steering_not_polled_between_tools holds.",
        ],
      },
      {
        id: "M2.3",
        title: "Tools, typed tools, registry, scheduler, cancellation joining",
        crate: "crates/pi-agent-core",
        read: "part 1 §4.5 (Tool, ToolOutput, TypedTool), §4.6 (the eleven phases, cancellation rules, CPU-bound note); part 2 §8.1 rows for beforeToolCall/afterToolCall/parallel/sequential/terminate, §9.1 (FuturesUnordered, no detached tasks), §9.2 (Tool and LocalTool); pi agent-loop.ts tool execution.",
        deliver: [
          "Tool / LocalTool traits, ToolUpdateSink / LocalToolUpdateSink, ToolOutput, ToolError, ToolCallContext, ToolRegistry, TypedTool<I, F> with JSON Schema validation → serde → typed call; schema validation against ToolSpec.",
          "ToolScheduler with PreflightIndex / CompletionIndex / SourceIndex; ToolExecutionPlan::SequentialBatch rule; ToolBatchOutcome.terminate = all(results.terminate); truncated-response rejection; child cancellation token per batch, joined before RunFinished.",
        ],
        accept: ["§10.9 Tools tests pass with their exact names, including tool_parallel_completion_events_are_completion_order, tool_parallel_result_messages_are_source_order, tool_length_truncated_calls_are_never_executed, tool_cancellation_joins_running_parallel_calls, tool_no_process_or_file_mutation_after_run_finished."],
      },
      {
        id: "M2.4",
        title: "Policies: ContextPolicy, MessageProjector, ToolPolicy, TurnPolicy; PreparedContext",
        crate: "crates/pi-agent-core",
        read: "part 1 §4.7 (policy traits, authorization is not sandboxing), §4.8 (compaction outside the core, PreparedContext with overrides); part 2 §8.1 rows for transformContext/convertToLlm/prepareNextTurn/prepareNextTurnWithContext/shouldStopAfterTurn, §4.4 (ContextPrepared carries HandoffReport — the report type lives in pi-ai, produced by Models; the agent emits what it receives).",
        deliver: [
          "ContextPolicy::prepare_agent_records, MessageProjector::project, ToolPolicy::authorize / finalize, TurnPolicy::prepare_next_turn / should_stop, PreparedContext{context, model_override, options_override}, default implementations matching pi's defaults (convertToLlm default projection).",
        ],
        accept: ["§10.9 Context phases tests pass with their exact names."],
      },
    ],
  },
  M3: {
    title: "Milestone 3 — Models control plane (part 1 §10 M3, §3.5–3.8; part 2 §2.4–2.6, §3, §4, §5, §6)",
    packages: [
      {
        id: "M3.1",
        title: "Provider composition, Models registry/router, request pipeline, middleware, retry",
        crate: "crates/pi-ai",
        read: "part 1 §3.5 (ProviderRegistration, ChatApi, compatibility over subclasses), §3.6 (Models handle, request path, builder, atomic registration); part 2 §2.4 (RetryPolicy, RetryClassifier, RetryDecision, ApiExecutionContext, establish_with_retry), §2.5 (HttpTransport, HeaderTransform, PayloadTransform/ErasedPayloadTransform, ResponseObserver, AttemptMiddleware), §2.6 (the thirteen-step ordering, Bedrock special case noted); pi models.ts, utils/provider-retry.ts, utils/headers.ts, types.ts options.",
        deliver: ["Every type and trait named in those sections; Models implements ModelRuntime; no registry lock held across awaits; retry loop cancellable in request and backoff; logical request frozen across attempts."],
        accept: ["§10.3 and §10.4 tests pass with their exact names against a fake HttpTransport."],
      },
      {
        id: "M3.2",
        title: "Catalogs: sources, stores, overrides, layers, refresh/persist/publish",
        crate: "crates/pi-ai",
        read: "part 1 §3.7; part 2 §5.3–5.7 (ProviderCatalogLayers, composition order, ModelCatalogSource, CatalogCandidate, ModelsStore, ModelOverrideStore, publish_candidate, host override updates, RefreshReport / ProviderRefreshResult); pi models.ts, models-store.ts, model-catalog.ts, providers/radius-config.ts.",
        deliver: ["Every type and trait named; persist-before-publish; generation checks; readers load Arc<CatalogSnapshot> atomically; in-memory ModelsStore and ModelOverrideStore."],
        accept: ["§10.7 Catalog tests pass with their exact names."],
      },
      {
        id: "M3.3",
        title: "Auth: resolvers, credential store and lease, interaction, redirect receivers, device code",
        crate: "crates/pi-ai",
        read: "part 1 §3.8 (AuthResolver, ResolvedAuth, CredentialStore, CredentialLease, precedence); part 2 §6.1–6.4 (AuthInteraction, AuthHostCapabilities, AuthPrompt/AuthAnswer/AuthEvent, RedirectReceiverRequest/RedirectStrategy/RedirectReceiver, responsibility split, racing callback and manual input), §6.6; pi auth/*.ts, auth/oauth/device-code.ts, oauth/pkce.ts.",
        deliver: ["Every type and trait named; in-memory CredentialStore with leases; PKCE/state helpers; RFC 8628 device-code polling rules from pi; select_first_valid race; AuthError::UnsupportedRedirectStrategy."],
        accept: ["§10.7 Authentication tests pass with their exact names using fake interactions and receivers."],
      },
      {
        id: "M3.4",
        title: "ApiFamily trait, erased handler, common planning, handoff policy and report",
        crate: "crates/pi-ai",
        read: "part 2 §3.1–3.4 (ownership, ApiFamily, SimpleLoweringContext, ErasedApiHandler, ApiOptionsInput precedence, CommonSimplePlan/plan_common with 4096 and 1024 constants, pi-equivalent token estimation), §3.7 (LevelSupport semantics, strict vs clamp); §4.2–4.4 (HandoffPolicy, HandoffResult/Report/Change, the eight-phase order, ToolCallIdPolicy, surfacing losses); pi api/simple-options.ts, api/transform-messages.ts, utils/estimate.ts and the Copilot transform test.",
        deliver: ["ApiFamily, ErasedApiHandler, plan_common and the common types; HandoffPolicy and the transform implementing all eight phases over canonical types (API-family final shaping via a trait hook filled in M4); HandoffReport on PreparedContext."],
        accept: ["§10.5 common tests (simple_* and reasoning_* / thinking_budget_*) and all §10.6 tests pass with their exact names."],
      },
    ],
  },
  M4: {
    title: "Milestone 4 — prove API/provider separation: one OpenAI-family and one Anthropic-family API, two providers sharing the OpenAI family, one Anthropic provider; wire and replay goldens (part 1 §10 M4; part 2 §1.4–1.5, §3.5–3.6, §10.8)",
    packages: [
      {
        id: "M4.1",
        title: "Ordered JSON wire writer and the pi fixture capture corpus",
        crate: "crates/pi-ai (+ providers/fixtures)",
        read: "part 2 §10.8 in full (byte-comparison contract, the JSON.stringify behaviors the writer must reproduce, deterministic injection, redaction, turn-two replay goldens); pi's request construction for openai-completions and anthropic-messages.",
        deliver: [
          "OrderedJsonValue / OrderedJsonObject / OrderedJsonArray and a writer reproducing JSON.stringify: insertion order, integer-like key ordering, no whitespace, omitted absent fields, exact escaping, pi-compatible number representation, surrogate sanitation (the legacy ai crate's utils/ecma_json.rs and utils/js_string.rs on branch legacy/pre-architecture-v2 are a quarry).",
          "A fixture capture tool (bun/TypeScript against the pinned pi at " + PI_AI + ", with a local capture server) that records, per family and per fixture case in §10.8's list, the canonical context, the pi request body bytes, pi's turn-1 response frames, and pi's turn-2 request body; secrets redacted, nondeterministic values injected; captured fixtures checked in under providers/fixtures/<family>/; a README explaining how to regenerate. Capture requires network and credentials: use what the environment provides (DEEPSEEK_API_KEY, GEMINI_API_KEY, OPENROUTER_API_KEY, the pi auth store at ~/.pi/agent/auth.json for anthropic/openai-codex/github-copilot OAuth) and record in the README exactly which cases were captured and which could not be and why.",
        ],
        accept: ["Writer tests cover every listed JSON.stringify behavior; at least the text-only, tool-call, tool-result, and reasoning-replay cases are captured for openai-completions and anthropic-messages."],
      },
      {
        id: "M4.2",
        title: "OpenAI Completions family: lowering, encoder, decoder with replay items; openai and a second provider sharing it",
        crate: "providers/pi-ai-openai (+ crates/pi-ai api family module)",
        read: "part 2 §1.5 (reasoning field names, reasoning_details as replay items, legacy fallback import), §3.6 (OpenAiCompletionsOptions, OpenAiReasoningPlan, resolve_compat from the effective base URL), §5.1 OpenAiCompletionsModelConfig; pi api/openai-completions.ts, openai-prompt-cache.ts, providers/openai.ts and one more openai-completions provider (deepseek or openrouter).",
        deliver: ["ApiFamily impl, decoder emitting the §1.5 events, encoder producing byte-identical bodies, two ProviderRegistrations sharing the implementation with compatibility profiles; catalog data for those providers from pi's published data."],
        accept: ["wire_openai_completions_pi_exact for every captured case; openai_chat_* §10.2 tests; openai_* §10.5 tests; openai_chat_reasoning_details_turn_two_pi_exact."],
      },
      {
        id: "M4.3",
        title: "Anthropic Messages family: lowering, encoder, decoder with signature replay items; anthropic provider",
        crate: "providers/pi-ai-anthropic (+ crates/pi-ai api family module)",
        read: "part 2 §1.4 (signature_delta, redacted thinking, encode_anthropic_thinking), §3.5 (AnthropicOptions, AnthropicThinking, lower_simple), §5.1 AnthropicMessagesModelConfig; pi api/anthropic-messages.ts, providers/anthropic.ts.",
        deliver: ["ApiFamily impl, decoder, encoder, ProviderRegistration; pi's temperature suppression and allow_empty_signature behavior."],
        accept: ["wire_anthropic_messages_pi_exact for every captured case; anthropic_* §10.2 and §10.5 tests; anthropic_signed_thinking_turn_two_pi_exact and anthropic_redacted_thinking_turn_two_pi_exact."],
      },
    ],
  },
  M5: {
    title: "Milestone 5 — persistent credentials and FFI (part 1 §10 M5, §6; part 2 §6.5)",
    packages: [
      {
        id: "M5.1",
        title: "File-backed credential leases, OAuth refresh locking, persisted credential format",
        crate: "crates/pi-ai",
        read: "part 1 §3.8 (lease semantics, failed refresh never falls back to env); part 2 §6.6; pi auth/credential-store.ts and auth/resolve.ts.",
        deliver: ["A file-backed CredentialStore with file locks implementing CredentialLease; refresh under lease; persisted format with schema_version."],
        accept: ["auth_oauth_refresh_is_serialized, auth_failed_oauth_refresh_never_falls_back_to_env, auth_login_persists_under_modify pass against the file store."],
      },
      {
        id: "M5.2",
        title: "pi-ffi: opaque handles, versioned event envelopes, cancellation, auth session state machine, one generated binding",
        crate: "bindings/pi-ffi",
        read: "part 1 §6 (FFI architecture, C-style surface, envelope, UniFFI, plugin boundary); part 2 §6.5 (auth session challenge/response protocol, PI_AUTH_CHALLENGE_SUPERSEDED), §9.4 (actor facade requirements).",
        deliver: ["The part 1 §6 C surface and JSON envelope; the part 2 §6.5 auth session API; a UniFFI binding for one target (Swift); no Rust trait objects, futures, streams, or Tokio types across the boundary."],
        accept: ["An example host drives a ScriptedRuntime-backed agent through the binding and receives sequenced envelopes; cancellation works; an auth session completes a scripted device-code flow."],
      },
    ],
  },
  M6: {
    title: "Milestone 6 — remaining API families and providers; wire gate (part 2 §1.6–1.8, §10.8 family list)",
    packages: [
      { id: "M6.1", title: "OpenAI Responses and OpenAI Codex Responses families + providers", crate: "providers/pi-ai-openai", read: "part 2 §1.6 (OpenAiResponsesReplay, output-item ordering, turn-two reconstruction); pi api/openai-responses.ts, openai-responses-shared.ts, openai-codex-responses.ts, providers/openai-codex.ts, auth/oauth/openai-codex.ts.", deliver: ["Both families, the codex transport, the codex OAuth flow via the M3.3 contracts, providers."], accept: ["wire_openai_responses_pi_exact, wire_openai_codex_responses_pi_exact, responses_* §10.2 tests, openai_responses_encrypted_reasoning_turn_two_pi_exact."] },
      { id: "M6.2", title: "Google Generative AI and Vertex families + providers", crate: "providers/pi-ai-google", read: "part 2 §1.8; pi api/google-shared.ts, google-generative-ai.ts, google-vertex.ts, providers/google.ts, providers/google-vertex.ts.", deliver: ["Both families, providers, Vertex credential resolution."], accept: ["wire_google_generative_ai_pi_exact, wire_google_vertex_pi_exact, google_* §10.2 tests, google_tool_thought_signature_turn_two_pi_exact, google_empty_signed_part_turn_two_pi_exact."] },
      { id: "M6.3", title: "Bedrock Converse Stream family + provider", crate: "providers/pi-ai-bedrock", read: "part 2 §1.7, §2.6 Bedrock signing special case; pi api/bedrock-converse-stream.ts, bedrock-provider.ts, providers/amazon-bedrock.ts.", deliver: ["Family, provider, logical-header insertion before SigV4 signing, response-header capture."], accept: ["wire_bedrock_converse_stream_pi_exact, bedrock_* §10.2 and §10.4 tests, bedrock_redacted_reasoning_turn_two_pi_exact."] },
      { id: "M6.4", title: "Azure OpenAI Responses, Mistral Conversations, pi-messages families; Cloudflare, Radius, and every remaining provider; credential-scoped availability; overflow classifier; pi-ai-providers-all", crate: "providers/*", read: "part 2 §10.8 family list, §5.7 (Radius as a ModelCatalogSource); pi api/azure-openai-responses.ts, mistral-conversations.ts, pi-messages.ts, cloudflare*.ts, providers/*.ts (github-copilot.ts in full: entitlement-driven model availability), providers/all.ts, remaining auth/oauth/*.ts flows; pi models.ts filterModels/getAvailable/checkAuth; pi utils/overflow.ts in full and its tests packages/ai/test/overflow.test.ts and context-overflow.test.ts.", deliver: ["Every remaining family and provider as leaf crates; OAuth flows for each provider through the M3.3 contracts; pi-ai-providers-all aggregator; catalogs from pi's published data.", "Credential-scoped availability on the Models control plane, matching pinned models.ts: filter_models (narrow the visible catalog per credential, including Copilot entitlement model lists per providers/github-copilot.ts), get_available (models whose auth is complete), check_auth (configuration check without network) — Send and Local, with named conformance tests and manifest mappings for the upstream cases that exercise them.", "The message-level overflow classifier ported pattern-for-pattern from pi utils/overflow.ts into crates/pi-ai: OVERFLOW_PATTERNS, NON_OVERFLOW_PATTERNS (non-overflow wins), silent-overflow heuristics, is_context_overflow(message, context_window); map packages/ai/test/overflow.test.ts and context-overflow.test.ts as semantic-parity (they are currently planned/M6 — this package makes that true)."], accept: ["Every wire_*_pi_exact suite in §10.8 passes for its captured corpus; the Wire gate and Replay gate are met.", "Availability and overflow-classifier tests pass with pi-cited bases; overflow.test.ts and context-overflow.test.ts are semantic-parity in the manifest.", "catalog_refresh_candidate_with_unregistered_api_rejects_publication: a dynamic-refresh candidate containing a model whose api has no registered implementation rejects the entire publication per the catalog-publish contract (part 2 §5.4–5.5)."] },
    ],
  },
  M7: {
    title: "Milestone 7 — deferred responses, sessions, environment, Tokio runtime (part 2 §7.2–7.6, §7.10, §9.4; pi deferred-response contract)",
    packages: [
      {
        id: "M7.0",
        title: "Deferred responses: serializable DeferredHandle, fetch/cancel on the execution seam, ScriptedRuntime support",
        crate: "crates/pi-ai",
        read: "pi types.ts DeferredHandle (types.ts:409), fetchDeferred/cancelDeferred on the API handler surface (types.ts:270–285), api/lazy.ts, the models.ts deferred routing, providers/faux.ts, and the upstream tests that exercise deferred responses (packages/ai/test/providers.test.ts, telemetry-options.test.ts). This surface is in the founding seams document but absent from parts 1–2; pi source is the sole behavior authority, and shapes follow the adopted architecture idioms (plain serializable data with schema_version, Send and Local families, BoxFuture aliases).",
        deliver: [
          "DeferredHandle as plain serializable data with the pinned fields (provider, model_id, api, id, expires_at, poll_after_ms, data) plus schema_version; survives persistence across process restarts.",
          "Optional fetch_deferred / cancel_deferred on the ApiFamily/handler surface and Models entry points routing to them, Send and Local, mirroring pinned models.ts; providers without support surface pi's unsupported error.",
          "AssistantFinishReason::Deferred round trip: a run finishing deferred yields a handle that, after a persistence round trip, resolves through fetch_deferred to the final assistant message with replay intact.",
          "ScriptedRuntime deferred scripting so the session/harness milestones can consume suspended-run resumption hermetically.",
          "Manifest updates mapping the upstream deferred-response cases.",
        ],
        accept: ["Named deferred tests with pi-cited bases pass hermetically (Send and Local); handle persistence round trip is covered; manifest maps the upstream cases."],
      },
      { id: "M7.1", title: "pi-agent-session: entry tree, lanes, operation records, storage traits, reducer, recovery, branching", crate: "crates/pi-agent-session", read: "part 2 §7.2–7.6 in full; pi harness/session/types.ts, state.ts, session.ts, context.ts, memory.ts and their tests.", deliver: ["Every type and trait named; the eight reducer invariants; RecoveryDecision; in-memory storage."], accept: ["§10.10 Reducer and session tree tests pass with their exact names."] },
      { id: "M7.2", title: "pi-agent-env and pi-agent-runtime-tokio: capability traits, Tokio environment, process execution, actor facade", crate: "crates/pi-agent-env, crates/pi-agent-runtime-tokio", read: "part 2 §7.10 (AgentEnvironment, AgentFileSystem, ProcessSpawner, RunningProcess, TerminationPolicy), §9.4 (TokioAgentHandle, serialized commands, bounded channels, no detached tool tasks), §9.6; pi harness/env/nodejs.ts and its test.", deliver: ["Traits in pi-agent-env; Tokio implementations and the actor in pi-agent-runtime-tokio; CapabilityUnavailable path."], accept: ["§10.10 env_* tests pass with their exact names; the actor processes the nine commands serially."] },
    ],
  },
  M8: {
    title: "Milestone 8 — pin refresh; harness: compaction, branch summaries, skills, templates, reference tools, telemetry, orchestration (part 2 §7.7–7.12)",
    packages: [
      {
        id: "M8.0",
        title: "Pin refresh: re-verify pi-ai against the new pin and regenerate the manifest",
        crate: "parity, crates/pi-ai, providers/*",
        read: "The pin has moved from c49906ec77788625aacbdc53ebca6fbe65bd20f5 to the commit named in the PIN constant (packages/agent is source-identical between the two; the whole delta is in packages/ai). Read the upstream diff for: src/api/openai-completions.ts, src/providers/cloudflare-ai-gateway.ts, scripts/generate-models.ts, the new scripts/openrouter-reasoning-options.ts; and the test delta: the new test/openrouter-reasoning-options.test.ts plus the modified openai-completions-reasoning-details.test.ts, openai-completions-tool-choice.test.ts, zai-coding-plan-models.test.ts.",
        deliver: [
          "Audit the pi-ai delta against the Rust port (openai-completions family, cloudflare-ai-gateway provider, any regenerated catalog data) and implement pi's new-pin behavior where they now differ; nothing outside the delta changes.",
          "parity/manifest.toml: upstream_commit set to the new pin; a mapping for the new upstream test file; the three modified upstream tests' mappings re-verified against their new content (strengthen Rust tests where the upstream tests grew).",
          "parity/upstream-tests.txt regenerated from the new pin; parity/check.sh passes against the new worktree.",
        ],
        accept: ["parity/check.sh passes with upstream_commit = the new pin and every upstream test file (including the new one) mapped; the affected wire/conformance suites still pass byte-identically for the fixture corpus."],
      },
      { id: "M8.1", title: "Compaction, branch summarization, HarnessContextPolicy, overflow retry", crate: "crates/pi-agent-harness", read: "part 2 §7.7–7.8; pi harness/compaction/*.ts and tests.", deliver: ["CompactionPolicy, BranchSummaryPolicy, HarnessContextPolicy, the preparation flow, overflow retry under the same operation."], accept: ["§10.10 compaction_* and branch_summary_* tests pass with their exact names."] },
      { id: "M8.2", title: "Skills, prompt templates, reference tools, file mutation queue, truncation, telemetry", crate: "crates/pi-agent-harness", read: "part 2 §7.9, §7.11, §7.12; pi harness/skills.ts, prompt-templates.ts, tools/*.ts, utils/truncate.ts, telemetry.ts, docs/telemetry-schema.md and tests.", deliver: ["SkillCatalog, PromptTemplateRegistry, FileMutationQueue, edit semantics, TruncationLimits, BashToolResultDetails, TelemetryEnvelope/TelemetryEvent/TelemetrySink with the listed defaults and a generated JSON Schema checked in."], accept: ["§10.10 mutation_queue_*, edit_*, truncate_*, bash_*, skill_*, prompt_template_*, telemetry_* tests pass with their exact names."] },
      { id: "M8.3", title: "Harness orchestration over agent-core + session + environment", crate: "crates/pi-agent-harness", read: "part 2 §7.1 (derive from the implemented subsystems, not the scaffold), §7.6, §8.4 (durable enqueue acknowledgement); pi harness/agent-harness.ts, events.ts, messages.ts, result.ts, reducer.ts and tests.", deliver: ["The harness operation lifecycle: operation records around runs, durable queue acknowledgement, recovery on open, resource formatting and events per pi."], accept: ["§10.10 session_open_operation_detected, session_multiple_open_operations_is_corruption, session_operation_recovery_reconstructs_intent pass; the Agent gate is met end-to-end through the harness."] },
    ],
  },
  M9: {
    title: "Milestone 9 — native durable session store (part 2 §7.5–7.6 storage semantics; §7.13 as amended by the owner ruling of 2026-08-25: no pi v4 backward compatibility — no existing consumers exist, so the native format stands alone)",
    packages: [
      {
        id: "M9.1",
        title: "File-backed native session storage: serialized append, sequence validation, torn-tail recovery, atomic rewrite, crash recovery; retire the compat crate",
        crate: "crates/pi-agent-session",
        read: "part 2 §7.5–7.6 (storage traits, operation recovery, the eight reducer invariants already implemented in M7.1) and §7.13's protocol-semantics analysis (serialized append, sequence validation, torn-tail recovery, atomic rewrite are storage protocol semantics, not JSONL syntax — they survive the ruling even though v4 byte compatibility does not); pi harness/session/jsonl/storage.ts and codec.ts as the BEHAVIOR basis for those semantics only (not for byte format); the upstream session behavior tests context.test.ts, memory.test.ts, search.test.ts.",
        deliver: [
          "A file-backed durable backend for the M7.1 storage traits in the native format (schema_version-carrying, append-only mutations): serialized append (one writer, append lock), sequence validation on read, torn-tail detection and repair on open, atomic rewrite for compaction/branch operations, and recovery that feeds RecoveryDecision exactly as the in-memory store does.",
          "The one-open-operation rule enforced on live append while replay preserves multiple unresolved starts for corruption diagnosis (same split M7.1 implemented, now durable).",
          "A reusable, backend-generic storage conformance harness exported by pi-agent-session behind a `conformance` feature — the Rust mirror of pi harness/session/testing/conformance.ts: it takes any SessionStorage/SessionRepository (and Local) implementation and runs the full §10.10 storage/recovery suite against it, so third-party pluggable backends validate identically. The in-memory and file backends are its first two consumers.",
          "Port the format-agnostic upstream session behavior tests (context, memory, search) through that harness against both backends; map them in the manifest.",
          "Delete crates/pi-agent-compat-pi-jsonl (crate and workspace membership) per the owner ruling; flip packages/agent/test/harness/session/jsonl-codec.test.ts, jsonl-storage.test.ts, and jsonl.test.ts to deliberate-divergence with reason: owner ruling 2026-08-25 — no backward compatibility (no existing consumers); the native durable store ports the protocol semantics, not the v4 byte format.",
        ],
        accept: ["§10.10 storage/recovery conformance names pass against the file backend (torn-tail, sequence validation, one-open-operation, recovery reconstructs intent); the behavior tests pass on both backends; the manifest has no remaining planned session entries and parity/check.sh passes with the compat crate gone."],
      },
    ],
  },
  GATES: {
    title: "Commitment gates — verify all four (part 2 'Commitment gates')",
    packages: [
      { id: "G.1", title: "Replay, Wire, Agent, and Session gates", crate: "workspace", read: "part 2 'Commitment gates', §10 in full; goal.md.", deliver: ["A gates report at docs/porting-pi-ai-and-agent-core-docs/gates-report-<date>.md listing, per gate, every named test and its status, every §10.8 family and fixture case, the manifest coverage by status, and every correction note added to the architecture documents during the build."], accept: ["All four gates pass; the manifest has no planned entries; parity/check.sh passes."] },
    ],
  },
};

const milestone = MILESTONES[MILESTONE];
if (!milestone) {
  log("✘ unknown milestone " + MILESTONE + "; known: " + Object.keys(MILESTONES).join(", "));
  return { ok: false, reason: "unknown milestone" };
}
const PACKAGES = ONLY ? milestone.packages.filter((p) => ONLY.includes(p.id)) : milestone.packages;

function implPrompt(pkg, feedback, attempt) {
  if (!feedback && attempt === 0 && INITIAL_FEEDBACK) feedback = INITIAL_FEEDBACK;
  return (
    "You are the IMPLEMENTER (" + MODEL + ", xhigh reasoning) working in " + REPO + ".\n\n" + COMMON + "\n" +
    "MILESTONE: " + milestone.title + "\n" +
    "PACKAGE " + pkg.id + " — " + pkg.title + "\nTarget crate(s): " + pkg.crate + "\n" +
    "READ IN FULL: " + pkg.read + "\n" +
    "DELIVER:\n" + pkg.deliver.map((d) => "- " + d).join("\n") + "\n" +
    "ACCEPTANCE (the reviewer runs exactly this):\n" + pkg.accept.map((a) => "- " + a).join("\n") + "\n\n" +
    (feedback
      ? "A REVIEWER REJECTED your previous attempt (round " + attempt + "). Your prior edits are still on disk; " +
        "address every point below without regressing what was already correct:\n" + feedback + "\n\n"
      : "") +
    "If the working tree contains uncommitted work, it is material, not truth: audit it against the architecture " +
    "and pi, keep what is right, fix the rest. Report, in plaintext: files added/changed; for each delivered item " +
    "the architecture section it realizes and any choice you had to make; the pi files read and any place the " +
    "architecture and pi disagreed (and the correction note you added); tests added with their §10 names and " +
    "manifest entries; each gate command and its observed result; anything the sandbox denied."
  );
}

function reviewPrompt(pkg, implReport) {
  return (
    "You are the REVIEWER (" + MODEL + ", xhigh reasoning) in " + REPO + ". You are a fresh, independent session: " +
    "you did not write this code. You do not modify, create, stage, or commit files; you read and run verification " +
    "commands.\n\n" + COMMON + "\n" +
    "MILESTONE: " + milestone.title + "\nPACKAGE " + pkg.id + " — " + pkg.title + "\nTarget crate(s): " + pkg.crate + "\n" +
    "READ IN FULL: " + pkg.read + "\nDELIVER:\n" + pkg.deliver.map((d) => "- " + d).join("\n") + "\n" +
    "ACCEPTANCE:\n" + pkg.accept.map((a) => "- " + a).join("\n") + "\n\n" +
    "The implementer reported:\n" + (implReport == null ? "(no report)" : String(implReport)) + "\n\n" +
    "Judge the actual working-tree changes (`git status`, `git diff`) as an adversary:\n" +
    "1. On target: every DELIVER item exists in the shape the architecture documents specify — names, fields, " +
    "variants, trait signatures, bounds, crate placement. A simplification, a merged crate, a renamed concept, or an " +
    "omitted property is a rejection.\n" +
    "2. Faithful to pi where the architecture maps to pi: read the cited pi files and compare behavior; any " +
    "divergence not on part 2 §10.11 is a rejection unless it carries a correction note that pi source supports.\n" +
    "3. Idiomatic and truthful per COMMON.\n" +
    "4. Tests: every ACCEPTANCE test exists under its exact §10 name, is hermetic, cites its pi basis, and passes; " +
    "the parity manifest is updated and parity/check.sh passes (once it exists).\n" +
    "5. Gates: run them yourself — cargo fmt --all -- --check; cargo clippy --workspace --all-targets -- -D warnings; " +
    "cargo build --workspace; cargo test --workspace; cargo doc --workspace --no-deps; bash parity/check.sh (if " +
    "present); git diff --check — and confirm only the allowed paths changed.\n" +
    "ok=true only if the package is complete, on target, faithful, and green. Otherwise ok=false with specific " +
    "file-and-line feedback — it is the implementer's only context next round."
  );
}

const REVIEW_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["ok", "feedback", "summary", "gatesRun", "blocking"],
  properties: {
    ok: { type: "boolean", description: "true only if complete, on target, faithful, and gates green" },
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
        properties: { file: { type: "string", description: "file:line" }, issue: { type: "string", description: "What is off target or wrong (cite the architecture section or pi file:line)" } },
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
  required: ["ok", "branch", "head", "clean", "dirtyOutsideAllowed", "piHead", "note"],
  properties: {
    ok: { type: "boolean", description: "true only if the branch matches, the tree state satisfies the instruction, and the pi worktree is at the pin" },
    branch: { type: "string", description: "git branch --show-current" },
    head: { type: "string", description: "git rev-parse HEAD" },
    clean: { type: "boolean", description: "git status --porcelain is empty" },
    dirtyOutsideAllowed: { type: "boolean", description: "true if any path in git status --porcelain is outside crates/, providers/, bindings/, examples/, parity/, docs/porting-pi-ai-and-agent-core-docs/, Cargo.toml, Cargo.lock" },
    piHead: { type: "string", description: "git rev-parse HEAD in the pi worktree" },
    note: { type: "string", description: "Anything off" },
  },
};

phase("Preflight");
log(milestone.title + " · " + PACKAGES.length + " package(s): " + PACKAGES.map((p) => p.id).join(", ") + " · " + MODEL + "/xhigh impl+review · ≤" + MAX_ROUNDS + " rounds · pin " + PIN.slice(0, 9));
const preflight = await agent(
  "Preflight only; change nothing. In " + REPO + ": report `git branch --show-current`, `git rev-parse HEAD`, and whether " +
    "`git status --porcelain` is empty. In " + PI_ROOT + ": report `git rev-parse HEAD`. ok=true only if the branch is '" +
    BRANCH + "', " + (ALLOW_DIRTY
      ? "every path in `git status --porcelain` is under crates/, providers/, bindings/, examples/, parity/, docs/porting-pi-ai-and-agent-core-docs/, or is Cargo.toml/Cargo.lock (uncommitted work there is expected this run; set clean=false and list anything outside those paths in note), "
      : "the tree is clean, ") + "and the pi HEAD is " + PIN + ".",
  { label: "preflight", phase: "Preflight", model: MODEL, mode: "read-only", cwd: REPO, configOptions: { reasoning_effort: "low" }, schema: PREFLIGHT_SCHEMA }
);
const preflightOk =
  !!preflight &&
  preflight.branch === BRANCH &&
  String(preflight.piHead || "").trim() === PIN &&
  (preflight.clean === true || (ALLOW_DIRTY && preflight.dirtyOutsideAllowed === false));
if (!preflightOk) {
  log("✘ preflight failed: " + JSON.stringify(preflight));
  return { ok: false, reason: "preflight", preflight };
}

phase("Build");
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
        phase: "Build",
        model: MODEL,
        mode: "agent-full-access",
        cwd: REPO,
        configOptions: XHIGH,
        retries: 1,
      }),
    (result) =>
      agent(reviewPrompt(pkg, result), {
        label: "review:" + pkg.id,
        phase: "Build",
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
    "In " + REPO + ", commit the package's work as ONE commit:\n" +
      "  git add crates providers bindings examples parity Cargo.toml Cargo.lock docs/porting-pi-ai-and-agent-core-docs .github\n" +
      "  git commit -m '" + MILESTONE + " " + pkg.id + ": " + String(pkg.title).replace(/'/g, "") + "'\n" +
      "Then read back the SHA with git rev-parse HEAD. If nothing is staged, make no commit. Do not modify any file, " +
      "amend history, push, or touch other branches.",
    { label: "commit:" + pkg.id, phase: "Build", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: { reasoning_effort: "low" }, schema: COMMIT_SCHEMA }
  );
  const c = commit || { committed: false, sha: "", note: "commit agent returned null" };
  const sha = c.committed === true && /^[0-9a-f]{7,40}$/i.test(String(c.sha).trim()) ? String(c.sha).trim() : null;
  log("✔ " + pkg.id + " approved after " + outcome.attempts + " round(s)" + (sha ? " · committed " + sha.slice(0, 9) : " · ⚠ no commit: " + c.note));
  results.push({ id: pkg.id, approved: true, rounds: outcome.attempts, sha, summary: verdict.summary || "" });
}

phase("Closeout");
const approved = results.filter((r) => r.approved);
const closeout = halted
  ? null
  : await agent(
      "You are the CLOSEOUT agent (" + MODEL + ", xhigh reasoning) in " + REPO + ".\n\n" + COMMON + "\n" +
        "Milestone " + MILESTONE + " packages were approved and committed (earlier runs first): " + JSON.stringify(PRIOR_APPROVED.concat(approved.map((r) => ({ id: r.id, sha: r.sha })))) + "\n" +
        "Do three things. (1) Run every gate once more on the final tree and bash parity/check.sh if present; report the " +
        "coverage summary. (2) Append a dated entry to " + DOCS + "/milestones.md (create it if absent) recording the " +
        "milestone, its packages and SHAs, the §10 tests now passing, manifest counts by status, and every correction note " +
        "added to the architecture documents. (3) Commit as ONE commit '" + MILESTONE + " closeout: milestone record' and report the SHA.",
      { label: "closeout", phase: "Closeout", model: MODEL, mode: "agent-full-access", cwd: REPO, configOptions: XHIGH, schema: COMMIT_SCHEMA, retries: 1 }
    );

return { ok: !halted, milestone: MILESTONE, pin: PIN, packages: results, closeout: closeout || null };
