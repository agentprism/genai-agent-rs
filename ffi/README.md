# genai-agent-ffi

UniFFI (Swift) bindings for `rust-genai-agent` — **Layer B** of
[`docs/embedding.md`](../docs/embedding.md). Ships as an XCFramework +
SwiftPM package that a SwiftUI app can embed directly.

## Surface

| Swift | Core |
| --- | --- |
| `Agent(setup:)` | `AgentBuilder::new(setup).build()` — env-resolved auth |
| `Agent.newWithApiKeys(setup:apiKeys:)` | same, with host-injected provider keys (`["openai": "…"]`) |
| `try await agent.prompt(_:)` / `continueRun()` / `waitForIdle()` | `Agent::prompt` / `continue_` / `wait_for_idle` |
| `agent.abort()` / `agent.signal()?.cancel()` | `Agent::abort` / `CancellationToken` |
| `agent.isStreaming()` / `agent.snapshot()` | `Agent::state` (the §8 render-from-state pattern) |
| `agent.subscribe(sink:)` / `subscribeJson(sink:)` | typed `AgentEvent`s / serde JSON (wire-format proof) |
| `agent.setTools(tools:)` / `addTool(tool:)` | host tools via the `AgentTool` callback interface |
| `agent.setBeforeToolCallHook(hook:)` … | all loop hooks as callback interfaces (`Before/AfterToolCall`, `Try*`, `TransformContext`, `ShouldStopAfterTurn`, `PrepareNextTurn`) |
| `agent.setSteeringSource(source:)` / `setFollowUpSource(source:)` | `QueueSource` message injection (steering mid-run, follow-up between runs); `nil` clears |

Everything is typed: `AgentSetup` (including `messages` and `chatOptions`),
the full message/event tree, `ToolSpec`/`ToolCallContext`/`AgentToolResult`,
hook contexts/outcomes, `AgentUsage`/`AgentCost`, and the config enums.
Free-form JSON crosses only in `…Json`-suffixed fields (`argsJson`,
`detailsJson`, `dataJson`, `argumentsJson`, `schemaJson`, `extraBodyJson`)
per the §8 naming rule.

**Deliberately out** (additive later, none breaking): host-side `StreamFn`
(providers go through `GenaiStreamFn`), `ConvertToLlm` (hosts keep the
default), per-tool `executionMode`, and the tool `UpdateSink`. Note: core
hook setters take a required `Arc` (no clear variant) except the queue
sources, so hooks can't currently be cleared once set.

## Semantics (from §8, resolved)

- **Runtime**: crate-owned multi-thread tokio runtime, lazy + idempotent,
  4 worker threads.
- **Callbacks**: fire on runtime worker threads; awaited sequentially (slow
  UI ⇒ natural backpressure). Hop to the main actor in your implementation.
- **Rendering**: events are signals; render from `agent.snapshot()` at
  display refresh for high-frequency UI.
- **Cancellation**: UniFFI futures aren't cancellable — use
  `agent.signal()?.cancel()` / `agent.abort()`. Tools/hooks receive an
  `AgentCancelToken` they should observe.
- **Error mapping**: thrown host errors surface as tool-execution or
  tool-hook errors in the loop; malformed `…Json` from a `BeforeToolCallHook`
  blocks the call with the parse error as the reason (safe failure).

## Build

```sh
./build_apple.sh                 # bindings → 3 release slices → XCFramework → swiftpm/
cd swiftpm && swift test         # offline package tests
cargo test -p genai-agent-ffi --features testing   # offline e2e (scripted stream, tools, hooks)
```

`build_apple.sh` verifies each XCFramework slice (presence, arch, exported
symbols for crash symbolication, deployment target) and fails loudly on
drift. Deployment targets default to 26.5 and are aligned across the script's
`IPHONEOS/MACOSX_DEPLOYMENT_TARGET` and `swiftpm/Package.swift` — override
the env vars for apps with a lower minimum (and lower `platforms:` in the
manifest to match).

## App integration (dev)

Add `ffi/swiftpm` as a local package dependency, link `GenAIAgent`, then:

```swift
import GenAIAgent

final class UI: AgentEventSink, @unchecked Sendable {
    func emit(event: AgentEvent) async { /* hop to MainActor, update state */ }
}

final class Weather: AgentTool, @unchecked Sendable {
    func spec() -> ToolSpec { /* name/label/description/schemaJson */ }
    func execute(call: ToolCallContext, cancel: AgentCancelToken) async throws -> AgentToolResult { /* … */ }
}

let agent = Agent.newWithApiKeys(
    setup: AgentSetup(model: "gpt-5-mini", systemPrompt: "…"),
    apiKeys: ["openai": "sk-…"]
)
agent.addTool(tool: Weather())
let sub = agent.subscribe(sink: UI())
try await agent.prompt("Plan my week")   // events stream into the sink
```
