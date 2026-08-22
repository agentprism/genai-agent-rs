export const meta = {
  name: "pi-agent-core-design",
  description:
    "Design the idiomatic Rust port of pi-agent-core on top of the agentprism-ai crate, to the standard in docs/porting-pi-ai-and-agent-core-docs/goal.md: behavioral inventories of every pi-agent-core module read from pi source, a consumer-surface check of every pi-ai surface pi-agent-core uses against the ai crate, three independent design candidates judged by a panel, and a synthesized design document plus a phased port plan. codex/gpt-5.6-sol at xhigh throughout; read-only except the final writer.",
  model: "codex/gpt-5.6-sol",
  phases: [
    { title: "Setup" },
    { title: "Inventory" },
    { title: "Consumer surface" },
    { title: "Design candidates" },
    { title: "Judge" },
    { title: "Synthesize" },
  ],
};

const A = typeof args === "string" ? JSON.parse(args) : args || {};
const REPO = A.repo || "/home/vikash/genai-agent/genai-agent-rs";
const PI_ROOT = A.piRoot || "/home/vikash/pi-pin-c49906ec7";
const PI_AI = PI_ROOT + "/packages/ai";
const PI_AGENT = PI_ROOT + "/packages/agent";
const PIN = A.pin || "c49906ec77788625aacbdc53ebca6fbe65bd20f5";
const DATE = A.date || "undated";
const GOAL = REPO + "/docs/porting-pi-ai-and-agent-core-docs/goal.md";
const SEAMS = REPO + "/docs/porting-pi-ai-and-agent-core-docs/v2/preserved-architectural-seams-pi-agent-core-v2.mdx";
const DESIGN = REPO + "/docs/porting-pi-ai-and-agent-core-docs/pi-agent-core-rust-design-" + DATE + ".md";
const MODEL = "codex/gpt-5.6-sol";
const XHIGH = { reasoning_effort: "xhigh" };

const COMMON =
  "GOAL (the owner's, and the only standard that matters): read " + GOAL + " in full before anything else. " +
  "In short — we are building a Rust crate that IS pi-agent-core, on top of the agentprism-ai crate at " + REPO +
  "/ai (which IS pi-ai): every feature and every observable behavior of pi-agent-core exists (public surface, " +
  "AgentMessage/AgentEvent protocol, the agent loop's ordering and cancellation semantics, hooks, steering and " +
  "follow-up queues, tool execution and validation, sessions and their persisted formats, compaction, reducers, " +
  "skills, prompt templates, telemetry, the harness environment contract, search), written as idiomatic Rust " +
  "(ownership, Result, traits, async, real types) that never impersonates the JS runtime. Litmus: any " +
  "pi-agent-core README example or test must be recreatable against the crate with the same observable " +
  "results, without a workaround. When idiom and parity seem to conflict, parity wins and idiom adapts; a " +
  "feature is never dropped for efficiency, convenience, 'no production consumer', or an earlier design " +
  "note. Anything genuinely unpreservable in Rust is a four-column row — Delta | Technical delta | pi " +
  "counterpart (file:line) | Why it cannot be preserved — for the owner to judge.\n\n" +
  "AUTHORITY: pi source at " + PI_AGENT + " (and " + PI_AI + " for the pi-ai surfaces it consumes), repo " +
  "pinned at " + PIN + ", is the only authority. " + SEAMS + " is background (it was corrected where it had " +
  "contradicted pi); the legacy crates at " + REPO + "/agent and " + REPO + "/genai and their docs are an " +
  "EARLIER, PARTIAL effort on a different foundation — they are not the design, not a baseline, and not " +
  "evidence; you may mine them for pi behaviors to double-check against pi source, nothing more.\n\n" +
  "OWNER RULINGS (fixed): not ported — the proxy protocol (packages/agent/src/proxy.ts and its tests), " +
  "Windows, and pi-ai's ruled-out modules (lazy loading, azure-openai-responses, mistral-conversations, " +
  "pi-messages, image generation). The SQLite session backend lives in a separate npm package and is out of " +
  "scope; the session backend contract it implements is in scope. pi-telemetry is a dependency of " +
  "pi-agent-core: decide and justify how the Rust port provides equivalent observable telemetry behavior.\n\n" +
  "YOU DO NOT MODIFY FILES unless your role says so.\n";

const INVENTORY_SCOPES = [
  { key: "types-index", title: "src/types.ts, src/index.ts, src/stream-fn.ts — the public surface and the AgentMessage/AgentEvent model", files: "src/types.ts, src/index.ts, src/stream-fn.ts, README.md (Core Concepts, Event Flow, Event Types, Agent Options, Agent State, Methods, Custom Message Types)" },
  { key: "agent-loop", title: "src/agent-loop.ts — the turn loop", files: "src/agent-loop.ts, test/agent-loop.test.ts, README.md (Event Flow, Steering and Follow-up, Tools, Error Handling, Low-Level API)" },
  { key: "agent", title: "src/agent.ts — the stateful Agent", files: "src/agent.ts, test/agent.test.ts, test/e2e.test.ts, README.md (Quick Start, Agent Options, Agent State, Methods, Control, Events)" },
  { key: "harness-core", title: "src/harness/agent-harness.ts, types.ts, events.ts, messages.ts, result.ts, reducer.ts, system-prompt.ts, prompt-templates.ts, skills.ts", files: "the listed files plus docs/harness.md and their tests under test/harness/" },
  { key: "harness-session", title: "src/harness/session/** — sessions, state, JSONL codec/repo/storage, memory backend, the conformance suite", files: "src/harness/session/**, test/harness/session/**" },
  { key: "harness-compaction", title: "src/harness/compaction/** — compaction and branch summarization", files: "src/harness/compaction/**, test/harness/compaction.test.ts, test/harness/branch-summarization.test.ts" },
  { key: "harness-tools-env", title: "src/harness/tools/**, src/harness/env/nodejs.ts, src/harness/utils/** — tools, the environment contract, shell output and truncation", files: "the listed files, src/node.ts, test/harness/tools.test.ts, test/harness/nodejs-env.test.ts, test/harness/truncate.test.ts" },
  { key: "telemetry-search", title: "src/harness/telemetry.ts, src/search/** — telemetry and search", files: "the listed files, docs/telemetry-schema.md, docs/search.md, scripts/generate-telemetry-docs.ts, test/harness/telemetry.test.ts" },
];

const INVENTORY_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["area", "coverage", "publicSurface", "behaviors", "persistedFormats", "piAiSurfacesUsed", "testsToRecreate", "openQuestions"],
  properties: {
    area: { type: "string", description: "The scope key" },
    coverage: { type: "string", description: "Files read in full; anything not covered" },
    publicSurface: { type: "array", items: { type: "string" }, description: "Every exported type/function/class with file:line and a one-line contract" },
    behaviors: { type: "array", items: { type: "string" }, description: "Every observable behavior with file:line — ordering rules, cancellation points, error text, defaults, fallbacks, edge cases" },
    persistedFormats: { type: "array", items: { type: "string" }, description: "Every on-disk or wire format with file:line and the exact shape rules (field names, optional-vs-absent, versioning)" },
    piAiSurfacesUsed: { type: "array", items: { type: "string" }, description: "Every import from @earendil-works/pi-ai this area uses, with how it is used" },
    testsToRecreate: { type: "array", items: { type: "string" }, description: "Every test case name in scope with what it pins (and whether it is hermetic)" },
    openQuestions: { type: "array", items: { type: "string" }, description: "Anything a designer must decide, with the pi lines that constrain it" },
  },
};

const GAPS_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["checked", "gaps"],
  properties: {
    checked: { type: "array", items: { type: "string" }, description: "Every pi-ai surface checked: pi-ai export → Rust counterpart (file:line) → same-behavior yes/no" },
    gaps: {
      type: "array",
      description: "pi-ai surfaces pi-agent-core relies on that the ai crate lacks or gets wrong",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["surface", "usedBy", "piAi", "rust", "problem", "severity"],
        properties: {
          surface: { type: "string", description: "The pi-ai export" },
          usedBy: { type: "string", description: "pi-agent-core file:line that uses it" },
          piAi: { type: "string", description: "pi-ai file:line" },
          rust: { type: "string", description: "ai crate file:line or 'absent'" },
          problem: { type: "string", description: "What differs" },
          severity: { type: "string", enum: ["blocking", "major", "minor"], description: "Per goal.md" },
        },
      },
    },
  },
};

const CANDIDATE_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["lens", "design", "crateLayout", "typeMapping", "riskiestDecisions", "cannotPreserve"],
  properties: {
    lens: { type: "string", description: "The lens you designed from" },
    design: { type: "string", description: "The full design in Markdown: crate(s) and module layout mirroring pi file-for-file, the AgentMessage/AgentEvent/AgentState types, the StreamFn boundary on the ai crate, the loop's async/cancellation model, hooks, queues, tool trait and validation, session backend trait and JSONL codec, compaction, reducers, skills, prompt templates, telemetry, environment contract, search, error model, and how each pi README example reads in Rust. Cite pi file:line throughout." },
    crateLayout: { type: "array", items: { type: "string" }, description: "Rust path ⇐ pi path, one per line" },
    typeMapping: { type: "array", items: { type: "string" }, description: "pi type → Rust type, one per line, with the idiom used (enum/trait/Arc/etc.)" },
    riskiestDecisions: { type: "array", items: { type: "string" }, description: "Decisions most likely to break parity or idiom, with the alternative you rejected" },
    cannotPreserve: { type: "array", items: { type: "string" }, description: "Four-column rows (pipe-separated) you believe are genuinely unpreservable — rare" },
  },
};

const JUDGMENT_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["scores", "bestIndex", "graftFromOthers", "parityHoles"],
  properties: {
    scores: {
      type: "array",
      description: "One entry per candidate, in input order",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["index", "parity", "idiom", "buildsOnAi", "recreatability", "risk", "total", "reason"],
        properties: {
          index: { type: "number", description: "Candidate index" },
          parity: { type: "number", description: "0–1: every pi-agent-core behavior present" },
          idiom: { type: "number", description: "0–1: idiomatic Rust, no runtime impersonation" },
          buildsOnAi: { type: "number", description: "0–1: depends only on the ai crate's public surface, as pi-agent-core depends on pi-ai" },
          recreatability: { type: "number", description: "0–1: README examples and tests recreate without workaround" },
          risk: { type: "number", description: "0–1 where 1 = lowest risk" },
          total: { type: "number", description: "Mean of the five" },
          reason: { type: "string", description: "Short justification citing pi lines" },
        },
      },
    },
    bestIndex: { type: "number", description: "Index of the strongest candidate" },
    graftFromOthers: { type: "array", items: { type: "string" }, description: "Specific ideas from the other candidates the synthesis should adopt" },
    parityHoles: { type: "array", items: { type: "string" }, description: "pi behaviors NO candidate handles, with pi file:line" },
  },
};

const PLAN_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["designPath", "summary", "piAiPrerequisites", "phases"],
  properties: {
    designPath: { type: "string", description: "Absolute path of the design document written" },
    summary: { type: "string", description: "Ten lines: the design's shape and its riskiest decisions" },
    piAiPrerequisites: { type: "array", items: { type: "string" }, description: "pi-ai gaps that must be fixed in the ai crate before or during the port, from the consumer-surface check" },
    phases: {
      type: "array",
      description: "Ordered port phases, each one coherent implementer session, in dependency order",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["id", "title", "scope", "acceptance", "estimate"],
        properties: {
          id: { type: "string", description: "e.g. A1" },
          title: { type: "string", description: "One line" },
          scope: { type: "string", description: "pi files → Rust files, self-contained, with pi file:line anchors" },
          acceptance: { type: "string", description: "pi tests and README examples that must pass/recreate" },
          estimate: { type: "string", enum: ["small", "medium", "large"], description: "Size" },
        },
      },
    },
  },
};

phase("Setup");
log("pi-agent-core design · pin " + PIN.slice(0, 9) + " · " + INVENTORY_SCOPES.length + " inventories · 3 candidates · 3 judges · " + MODEL + "/xhigh");

phase("Inventory");
const inventories = (
  await parallel(
    INVENTORY_SCOPES.map((scope) => () =>
      agent(
        "You are a pi-agent-core INVENTORY reader (" + MODEL + ", xhigh reasoning). Working directory: " + PI_AGENT + ".\n\n" +
          COMMON + "\nSCOPE: " + scope.title + "\nFILES (read in full): " + scope.files + "\n\n" +
          "Produce a complete behavioral inventory of this scope from pi source — not a summary: every export, every " +
          "observable behavior, every persisted format, every pi-ai surface used, every test case. A designer who has " +
          "not read the files must be able to design from your inventory alone. Cite file:line everywhere.",
        { label: "inventory:" + scope.key, phase: "Inventory", model: MODEL, mode: "read-only", cwd: PI_AGENT, configOptions: XHIGH, schema: INVENTORY_SCHEMA, retries: 1 }
      )
    )
  )
).filter(Boolean);
log("inventory: " + inventories.length + "/" + INVENTORY_SCOPES.length + " scopes · " + inventories.reduce((n, i) => n + i.behaviors.length, 0) + " behaviors · " + inventories.reduce((n, i) => n + i.testsToRecreate.length, 0) + " tests");

phase("Consumer surface");
const surfaces = Array.from(new Set(inventories.flatMap((i) => i.piAiSurfacesUsed)));
const gaps = await agent(
  "You are the CONSUMER-SURFACE checker (" + MODEL + ", xhigh reasoning). Working directory: " + REPO + ".\n\n" +
    COMMON + "\n" +
    "pi-agent-core imports these pi-ai surfaces (collected from the inventories):\n" + surfaces.map((s) => "- " + s).join("\n") + "\n\n" +
    "Also grep " + PI_AGENT + "/src for every `from \"@earendil-works/pi-ai\"` import to make sure nothing is missed. For " +
    "each surface: find pi-ai's definition (" + PI_AI + "/src), find the ai crate's counterpart (" + REPO + "/ai/src, " +
    "lib.rs re-exports), and judge whether a Rust pi-agent-core could use it exactly as the TypeScript does — same " +
    "fields (including event.partial), same semantics, same errors. Report every gap; these become prerequisites.",
  { label: "consumer-surface", phase: "Consumer surface", model: MODEL, mode: "read-only", cwd: REPO, configOptions: XHIGH, schema: GAPS_SCHEMA, retries: 1 }
);
log("consumer surface: " + (gaps ? gaps.checked.length + " checked, " + gaps.gaps.length + " gaps" : "checker returned nothing"));

phase("Design candidates");
const LENSES = [
  { key: "types-first", lens: "TYPES-FIRST: start from pi's types.ts and the event/message protocol; derive every module from the data model; make illegal states unrepresentable without changing any observable shape." },
  { key: "runtime-first", lens: "RUNTIME-FIRST: start from the agent loop's async/cancellation/ordering semantics (agent-loop.ts, agent.ts) and the StreamFn boundary on the ai crate; derive the type and trait surface from what the loop needs; make every ordering rule and cancellation point explicit." },
  { key: "persistence-first", lens: "PERSISTENCE-FIRST: start from sessions, JSONL codec/repo/storage, state, compaction, and reducers — the formats a pi TS session must read back byte-compatibly in Rust and vice versa; derive the rest around that contract." },
];
const inventoryText = JSON.stringify(inventories, null, 1);
const gapsText = JSON.stringify(gaps || { checked: [], gaps: [] }, null, 1);
const candidates = (
  await parallel(
    LENSES.map((l) => () =>
      agent(
        "You are a DESIGNER (" + MODEL + ", xhigh reasoning). Working directory: " + REPO + ".\n\n" + COMMON + "\n" +
          "YOUR LENS: " + l.lens + "\n\n" +
          "Inputs — behavioral inventories of pi-agent-core (read them fully; open pi source wherever you need more):\n" + inventoryText + "\n\n" +
          "pi-ai consumer-surface gaps in the ai crate (design against pi-ai's real surface and list these as prerequisites; do not design around them):\n" + gapsText + "\n\n" +
          "Produce a complete design for the Rust pi-agent-core crate on the ai crate. It must account for every " +
          "behavior in the inventories; where you cannot, say so as a four-column row. Show how each pi README example " +
          "reads in Rust. Be concrete: module paths, type definitions (sketches), trait signatures, async model, " +
          "error types, persisted format handling.",
        { label: "design:" + l.key, phase: "Design candidates", model: MODEL, mode: "read-only", cwd: REPO, configOptions: XHIGH, schema: CANDIDATE_SCHEMA, retries: 1 }
      )
    )
  )
).filter(Boolean);
log("candidates: " + candidates.length + "/" + LENSES.length);
if (!candidates.length) {
  log("✘ no design candidates produced");
  return { pin: PIN, inventories, gaps, candidates: [], design: null };
}

phase("Judge");
const candidatesText = candidates.map((c, i) => "### Candidate " + i + " (" + c.lens.split(":")[0] + ")\n" + c.design + "\n\nCrate layout:\n" + c.crateLayout.join("\n") + "\n\nType mapping:\n" + c.typeMapping.join("\n") + "\n\nRiskiest decisions:\n" + c.riskiestDecisions.join("\n") + "\n\nCannot-preserve rows:\n" + c.cannotPreserve.join("\n")).join("\n\n");
const JUDGE_LENSES = ["parity completeness against the inventories and pi source", "idiomatic Rust and buildability on the ai crate's real surface", "recreatability of pi-agent-core's README examples and tests, and persisted-format compatibility"];
const judgments = (
  await parallel(
    JUDGE_LENSES.map((focus, index) => () =>
      agent(
        "You are a JUDGE (" + MODEL + ", xhigh reasoning) focusing on: " + focus + ". Working directory: " + REPO + ".\n\n" + COMMON + "\n" +
          "Behavioral inventories (ground truth, with pi source available to you):\n" + inventoryText + "\n\n" +
          "Candidates:\n" + candidatesText + "\n\n" +
          "Score every candidate 0–1 on parity, idiom, buildsOnAi, recreatability, and risk; name the best; list what " +
          "the synthesis should graft from the others; and list every pi behavior NO candidate handles.",
        { label: "judge:" + (index + 1), phase: "Judge", model: MODEL, mode: "read-only", cwd: REPO, configOptions: XHIGH, schema: JUDGMENT_SCHEMA, retries: 1 }
      )
    )
  )
).filter(Boolean);
const totals = candidates.map((_, i) => judgments.reduce((sum, j) => sum + ((j.scores.find((s) => s.index === i) || {}).total || 0), 0) / Math.max(1, judgments.length));
const bestIndex = totals.indexOf(Math.max(...totals));
log("judge: totals " + totals.map((t) => t.toFixed(2)).join(" / ") + " → best candidate " + bestIndex);

phase("Synthesize");
const plan = await agent(
  "You are the SYNTHESIZER (" + MODEL + ", xhigh reasoning). Working directory: " + REPO + ".\n\n" + COMMON + "\n" +
    "You MAY write exactly one file: " + DESIGN + " (create it). Do not modify anything else.\n\n" +
    "The winning candidate is index " + bestIndex + " (panel totals: " + totals.map((t) => t.toFixed(2)).join(", ") + ").\n\n" +
    "Candidates:\n" + candidatesText + "\n\n" +
    "Judgments:\n" + JSON.stringify(judgments, null, 1) + "\n\n" +
    "pi-ai consumer-surface gaps:\n" + gapsText + "\n\n" +
    "Behavioral inventories:\n" + inventoryText + "\n\n" +
    "Write the design document: (1) header with the goal doc, pin " + PIN + ", date " + DATE + ", method; " +
    "(2) the design — the winner with every graft the judges asked for and every parity hole they found closed, " +
    "organized by pi module in pi's file order, each section citing pi file:line and showing the Rust types/traits; " +
    "(3) how every pi-agent-core README example reads in Rust; (4) the pi-ai prerequisites (gaps the ai crate must " +
    "fix first); (5) the four-column CANNOT-PRESERVE table with only rows you judge genuinely unpreservable; " +
    "(6) the phased port plan — each phase one coherent implementer session with scope, acceptance (pi tests and " +
    "README examples), and size, in dependency order, suitable for a gated implement/review/commit workflow like " +
    "workflows/pi-ai-parity-remediation.workflow.js. Then return the structured plan.",
  { label: "synthesize", phase: "Synthesize", model: MODEL, mode: "agent", cwd: REPO, configOptions: XHIGH, schema: PLAN_SCHEMA, retries: 1 }
);
if (!plan) {
  log("✘ synthesizer returned nothing");
  return { pin: PIN, gaps, candidates, judgments, design: null };
}
log("design → " + plan.designPath + " · " + plan.phases.length + " phases · " + plan.piAiPrerequisites.length + " pi-ai prerequisites");
return { pin: PIN, designPath: plan.designPath, summary: plan.summary, piAiPrerequisites: plan.piAiPrerequisites, phases: plan.phases, gaps: gaps ? gaps.gaps : [], panelTotals: totals };
