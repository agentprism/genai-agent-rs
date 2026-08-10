# genai-agent-ffi

UniFFI (Swift) bindings for `rust-genai-agent` — **Layer B1** of
[`docs/embedding.md`](../docs/embedding.md). Ships as an XCFramework +
SwiftPM package that hosts a SwiftUI app can embed directly.

## B1 surface

| Swift | Core |
| --- | --- |
| `Agent(setup:)` | `AgentBuilder::new(setup).build()` (default stream fn) |
| `try await agent.prompt(_:)` | `Agent::prompt` (text input) |
| `try await agent.continueRun()` | `Agent::continue_` |
| `await agent.waitForIdle()` | `Agent::wait_for_idle` |
| `agent.abort()` / `agent.signal()?.cancel()` | `Agent::abort` / `CancellationToken` |
| `agent.isStreaming()` / `agent.snapshot()` | `Agent::state` (render-friendly `AgentSnapshot`) |
| `agent.subscribe(sink:)` | `EventSink` — **typed** `AgentEvent`s (the contract) |
| `agent.subscribeJson(sink:)` | same events as serde JSON (wire-format proof) |

Everything is typed: `AgentEvent`, `AgentMessage`, `AssistantMessage`,
`AssistantMessageEvent`, `AgentUsage`/`AgentCost`, `StopReason`, the config
enums, and `AgentSetup`. Free-form JSON crosses only in `…Json`-suffixed
fields (`argsJson`, `detailsJson`, `dataJson`, `argumentsJson`,
`initialMessagesJson`) per the §8 naming rule.

Out of scope for B1 (per §7/§8): host-implemented tools/hooks (B2), host-side
`StreamFn`, typed `chat_options`, typed initial messages.

## Semantics (from §8, resolved)

- **Runtime**: crate-owned multi-thread tokio runtime, lazy + idempotent
  (first use starts it), 4 worker threads.
- **Callbacks**: fire on runtime worker threads; `emit` is awaited
  sequentially per event (slow UI ⇒ natural backpressure). Hop to the main
  actor in your sink.
- **Rendering**: events are signals; for high-frequency UI render from
  `agent.snapshot()` at display refresh.
- **Cancellation**: UniFFI futures aren't cancellable — use
  `agent.signal()?.cancel()` / `agent.abort()`.

## Build

```sh
./build_apple.sh                 # bindings → 3 release slices → XCFramework → swiftpm/
cd swiftpm && swift test         # offline package tests
cargo test -p genai-agent-ffi --features testing   # offline e2e (scripted stream)
```

`build_apple.sh` verifies each XCFramework slice (presence, arch, exported
symbols for crash symbolication, deployment target) and fails loudly on
drift. Deployment targets (26.5) are aligned across the script's
`IPHONEOS/MACOSX_DEPLOYMENT_TARGET` and `swiftpm/Package.swift`.

## App integration (dev)

Add `ffi/swiftpm` as a local package dependency, link `GenAIAgent`, then:

```swift
import GenAIAgent

final class UI: AgentEventSink, @unchecked Sendable {
    func emit(event: AgentEvent) async { /* hop to MainActor, update state */ }
}

let agent = try Agent(setup: AgentSetup(model: "gpt-5-mini", systemPrompt: "…"))
let sub = agent.subscribe(sink: UI())
try await agent.prompt("Plan my week")   // events stream into the sink
```

Provider auth comes from the environment by default
(`OPENAI_API_KEY`, …). On-device key injection lands with the B2 surface.
