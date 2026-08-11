# Using `GenAIAgent` from Swift

A consumer's guide to the Swift package produced by `ffi/` (see
[`embedding.md`](embedding.md) for the design contract). Everything here was
verified against a real SwiftUI app integration — including the mistakes.

**Contents:** build & integration → creating an agent → events → streaming
UI pattern → cancellation → tools & hooks → gotchas.

**Design intent:** the package's callback protocols are `AnyObject & Sendable`
with `async` methods — satisfiable by `final class` types with checked
`Sendable` conformance. Consumer code should never need `@unchecked Sendable`;
the only place it appears anywhere in this stack is inside the
UniFFI-generated object classes, where the Rust side (`Arc`/`Mutex`) enforces
thread safety across the FFI boundary. If you find yourself reaching for
`@unchecked` to satisfy the protocols, that's a package bug worth raising,
not a consumer hack to ship.

---

## 1. Build & integrate

```sh
ffi/build_apple.sh        # bindings → 3 release slices → XCFramework → ffi/swiftpm/
```

Run this first (and after every pull that touches `ffi/src`): the package
references `GenAIAgent.xcframework` and the generated
`genai_agent_ffi.swift`, both of which only exist after a build.

In Xcode: **File → Add Package Dependencies… → Add Local…** → select
`ffi/swiftpm`, then add the **GenAIAgent** product to your app target's
*Frameworks, Libraries, and Embedded Content*.

You do **not** need to add any frameworks or linker flags — the XCFramework's
modulemap auto-links `Security`, `CoreFoundation`, and
`SystemConfiguration`.

Two environment notes:

- **Deployment targets**: the binary is built for iOS/macOS 26.5 by default.
  For a lower minimum, build with
  `IPHONEOS_DEPLOYMENT_TARGET=17.0 MACOSX_DEPLOYMENT_TARGET=14.0 ffi/build_apple.sh`
  and lower `platforms:` in `ffi/swiftpm/Package.swift` to match (keep all
  three in sync — misalignment surfaces as cryptic linker errors).
- **macOS App Sandbox**: if your Mac app has the App Sandbox enabled, it also
  needs the **Outgoing Connections (Client)** capability or every provider
  call fails at runtime. iOS needs nothing.

## 2. Creating an agent

```swift
import GenAIAgent

// On device — keys come from your app's secret storage:
let agent = Agent.newWithApiKeys(
    setup: AgentSetup(model: "gpt-5-mini", systemPrompt: "You are concise"),
    apiKeys: ["openai": "sk-…", "anthropic": "…"]
)

// On CLI/tests — genai resolves provider keys from the environment:
let agent = Agent(setup: AgentSetup(model: "gpt-5-mini"))
```

`AgentSetup` is fully typed data; the convenience init in the package gives
every field a default, so you only name what you need (`model` is required —
there is deliberately no default model).

## 3. Events: the sink contract

Subscribe once, receive typed events:

```swift
// A final class whose only stored property is an immutable MainActor
// closure: Sendable (required by AgentEventSink) is fully checked —
// no @unchecked needed in consumer code.
final class Forwarder: AgentEventSink {
    let handler: @MainActor (AgentEvent) -> Void
    init(handler: @escaping @MainActor (AgentEvent) -> Void) { self.handler = handler }
    func emit(event: AgentEvent) async { await handler(event) }
}

// Wire the handler with `self` capture AFTER the owning object is fully
// initialized (see §4 — `@Observable` types need `lazy` + `@ObservationIgnored`).
let subscription = agent.subscribe(sink: Forwarder { [weak self] event in
    self?.handle(event: event)
})
```

The semantics (from §8 of the design doc, all verified in practice):

- **Threading**: `emit` fires on the bridge's tokio worker threads. Hop to the
  main actor before touching UI state (as above).
- **Ordering/backpressure**: `emit` is *awaited sequentially* — the loop does
  not advance until your `emit` returns. Awaiting the main-actor hop (rather
  than fire-and-forget `Task { }`) preserves this: a busy UI slows the loop
  instead of dropping events.
- **Self-synchronizing events**: `messageUpdate` carries the *full partial
  message*, not a delta. Render it directly — never stitch chunks:

```swift
case .messageUpdate(let message, _):
    if case .assistant(let assistant) = message {
        text = assistant.text        // complete so far
        reasoning = assistant.thinking
    }
```

- **High-frequency rendering**: for very fast streams, treat events as signals
  and render from `agent.snapshot()` at display refresh instead of per event.
- `subscribeJson(sink:)` delivers the same events as serde JSON strings —
  useful for logging/replay; the typed sink is the primary path.

## 4. A complete SwiftUI view model

This is the pattern running in production-shaped code — copy it:

```swift
import Foundation
import GenAIAgent

@MainActor @Observable
final class ChatModel {
    enum State: Equatable {
        case idle, connecting, streaming, done
        case failed(String)
    }

    private(set) var state: State = .idle
    private(set) var text = ""
    private(set) var reasoning = ""
    private(set) var usageSummary: String?

    private let agent: Agent
    private var promptTask: Task<Void, Never>?

    /// Lazily subscribed at the end of `init`, so the event closure may
    /// capture `self` after every stored property is initialized.
    /// `@ObservationIgnored`: not view state — and @Observable can't
    /// transform `lazy`.
    @ObservationIgnored
    private lazy var subscription: Subscription = agent.subscribe(
        sink: EventForwarder { [weak self] event in self?.handle(event: event) }
    )

    var isActive: Bool { state == .connecting || state == .streaming }

    init(apiKeys: [String: String], model: String) {
        agent = Agent.newWithApiKeys(setup: AgentSetup(model: model), apiKeys: apiKeys)
        _ = subscription   // start the event stream
    }

    isolated deinit { promptTask?.cancel() }   // Swift 6.1+

    func send(prompt: String) {
        promptTask?.cancel()
        state = .connecting
        text = ""; reasoning = ""; usageSummary = nil

        promptTask = Task { [weak self, agent] in
            guard let self else { return }
            do {
                try await withTaskCancellationHandler {
                    try await agent.prompt(text: prompt)
                } onCancel: {
                    agent.abort()   // UniFFI futures aren't cancellable; abort() is.
                }
                if !Task.isCancelled { state = .done }
            } catch {
                if !Task.isCancelled { state = .failed(error.localizedDescription) }
            }
        }
    }

    func stop() {
        promptTask?.cancel()   // onCancel aborts the run
        promptTask = nil
        state = .idle
    }

    private func handle(event: AgentEvent) {
        switch event {
        case .messageUpdate(let message, _):
            if case .assistant(let assistant) = message {
                if state == .connecting { state = .streaming }
                text = assistant.text
                reasoning = assistant.thinking
            }
        case .messageEnd(let message):
            if case .assistant(let assistant) = message {
                text = assistant.text
                let usage = assistant.usage
                if usage.inputTokens + usage.outputTokens > 0 {
                    usageSummary = usage.summary
                }
            }
        default:
            break
        }
    }
}

private final class EventForwarder: AgentEventSink {
    let handler: @MainActor (AgentEvent) -> Void
    init(handler: @escaping @MainActor (AgentEvent) -> Void) { self.handler = handler }
    func emit(event: AgentEvent) async { await handler(event) }
}
```

Why the pieces are shaped this way:

- `withTaskCancellationHandler` bridges Swift task cancellation to the run's
  cancellation token — the Stop button is just `promptTask?.cancel()`.
- The sink is a `final class` with one immutable MainActor closure, so
  `Sendable` (required by `AgentEventSink`) is fully compiler-checked. The
  only `@unchecked Sendable` anywhere in this stack lives *inside* the
  UniFFI-generated object classes, where thread safety is enforced by Rust's
  `Arc`/`Mutex` on the other side of the FFI — the intended use of the escape
  hatch. Consumer code needs none of it.
- The handler captures `self` weakly; `lazy` defers the closure to first
  access (after `init`), and `@ObservationIgnored` opts the subscription out
  of `@Observable`'s tracking (which otherwise can't transform `lazy`).
- `isolated deinit` lets deinit touch MainActor-isolated state.

## 5. Tools & hooks

Implement a tool in Swift, register it, done:

```swift
final class Weather: AgentTool {   // final + immutable state = checked Sendable
    func spec() -> ToolSpec {
        ToolSpec(
            name: "get_weather",
            label: "Get Weather",
            description: "Current weather for a city",
            schemaJson: #"{"type":"object","properties":{"city":{"type":"string"}}}"#,
            strict: nil
        )
    }

    func execute(call: ToolCallContext, cancel: AgentCancelToken) async throws -> AgentToolResult {
        // call.argsJson is JSON text — decode with JSONDecoder/JSONSerialization
        AgentToolResult(
            content: [.text(text: "sunny, 24°C")],
            detailsJson: "{}",
            usage: nil,
            addedToolNames: [],
            terminate: false
        )
    }
}

agent.addTool(tool: Weather())
```

Hooks are the same shape — e.g. an approval gate:

```swift
final class Approval: BeforeToolCallHook {
    // Mutable state protected by the stdlib mutex (Swift 6 Synchronization).
    // Mutex is Sendable, so the class stays fully checked-Sendable.
    private let enabled = Mutex(true)

    func before(ctx: BeforeToolCallContext, cancel: AgentCancelToken) async -> BeforeToolCallOutcome {
        guard enabled.withLock({ $0 }) else { return BeforeToolCallOutcome(argsJson: nil, decision: nil) }
        let allow = askUser(ctx.tool_call.name) // your UX here
        return BeforeToolCallOutcome(
            argsJson: nil,
            decision: BeforeToolCallResult(block: !allow, reason: allow ? nil : "denied", terminate: false)
        )
    }
}
try agent.setBeforeToolCallHook(hook: Approval())
try agent.setBeforeToolCallHook(hook: nil)   // hooks clear with nil
```

`try` because hook registration fails with `AgentError.busy` while a run is
in flight — register before prompting (or after `waitForIdle()`).

Also available: `setAfterToolCallHook`, `setTryBeforeToolCallHook`,
`setTryAfterToolCallHook`, `setTransformContextHook`,
`setShouldStopAfterTurnHook`, `setPrepareNextTurnHook`, and
`setSteeringSource` / `setFollowUpSource` for queue-based message injection.

## 6. Gotchas (each one cost us time)

- **Named constructors are static methods.** Only `new` becomes `init` —
  it's `Agent.newWithApiKeys(setup:apiKeys:)`, not `Agent(setup:apiKeys:)`.
- **Rust parameter names become Swift labels**: `agent.prompt(text: "…")`.
- **Records need every member** in their generated init. Use the convenience
  inits in `GenAIAgentConvenience.swift` (`AgentSetup(model:)`,
  `ChatOptions(captureUsage: true)`, …) instead of spelling out 20 fields.
- **`…Json` fields are JSON text**, not decoded values (`argsJson`,
  `detailsJson`, `schemaJson`, `extraBodyJson`). Decode/encode with your
  usual JSON tools; the suffix is deliberate so this is never a surprise.
- **Run `ffi/build_apple.sh` before adding the package to Xcode** and after
  pulling changes that touch `ffi/src` — the XCFramework and generated Swift
  are build products, not committed.
- **Wire sink handlers after full initialization** — capturing `self` (even
  weakly) before every stored property has a value is a compile error.
  Assign the handler after `subscribe`, as in §4.
- **Sink/tool/hook types are `AnyObject & Sendable`.** Conform with
  `final class` + immutable state for checked conformance — no `@unchecked`
  needed. For mutable state (e.g. an approval toggle), protect it with
  `Synchronization.Mutex` rather than reaching for `@unchecked Sendable`; the
  only `@unchecked Sendable` in this stack lives inside the generated object
  classes (Rust-side `Arc`/`Mutex` enforces safety).
- **Cancellation is `agent.abort()` / `agent.signal()?.cancel()`**, not task
  cancellation alone — UniFFI futures don't observe Swift task cancellation,
  which is why §4 pairs them with `withTaskCancellationHandler`.
- **Errors** thrown from the bridge are `AgentError` and conform to
  `LocalizedError` — `error.localizedDescription` carries the core message.

## 7. Testing

- **Package**: `cd ffi/swiftpm && swift test` — offline; covers construction,
  sink/tool/hook registration, cancellation token, error mapping.
- **Bridge**: `cargo test -p genai-agent-ffi --all-features --locked` —
  offline end-to-end through the FFI objects using scripted provider streams
  (no network, no API keys), including host tool execution, hook
  blocking/clearing, and steering.
- **Your app**: construct the `Agent` in tests freely — it does no I/O until
  `prompt`.
