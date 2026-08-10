import XCTest
@testable import GenAIAgent

/// Offline tests: exercise the FFI surface (construction, sink/tool/hook
/// registration, cancellation token) without network access. The loop itself
/// is exercised Rust-side by `cargo test -p genai-agent-ffi --features testing`,
/// which drives a scripted provider stream through these same objects,
/// including host tool execution and hook blocking.
final class GenAIAgentTests: XCTestCase {

	// Callbacks fire on tokio worker threads, so sinks guard their state.
	final class CollectSink: AgentEventSink, @unchecked Sendable {
		private let lock = NSLock()
		private var _events: [AgentEvent] = []
		var events: [AgentEvent] { lock.withLock { _events } }
		func emit(event: AgentEvent) async {
			lock.withLock { _events.append(event) }
		}
	}

	final class CollectJsonSink: AgentEventJsonSink, @unchecked Sendable {
		private let lock = NSLock()
		private var _payloads: [String] = []
		var payloads: [String] { lock.withLock { _payloads } }
		func emitJson(eventJson: String) async {
			lock.withLock { _payloads.append(eventJson) }
		}
	}

	final class EchoTool: AgentTool, @unchecked Sendable {
		func spec() -> ToolSpec {
			ToolSpec(
				name: "echo",
				label: "Echo",
				description: "Echoes its input",
				schemaJson: #"{"type":"object","properties":{"text":{"type":"string"}}}"#,
				strict: nil
			)
		}

		func execute(call: ToolCallContext, cancel _: AgentCancelToken) async throws -> AgentToolResult {
			AgentToolResult(
				content: [.text(text: "echo: \(call.argsJson)")],
				detailsJson: "{}",
				usage: nil,
				addedToolNames: [],
				terminate: false
			)
		}
	}

	final class AllowAll: BeforeToolCallHook, @unchecked Sendable {
		func before(ctx _: BeforeToolCallContext, cancel _: AgentCancelToken) async -> BeforeToolCallOutcome {
			BeforeToolCallOutcome(argsJson: nil, decision: nil)
		}
	}

	func testAgentConstructsFromSetup() {
		_ = Agent(setup: AgentSetup(model: "gpt-5-mini", systemPrompt: "You are concise"))
	}

	func testAgentConstructsWithApiKeys() {
		_ = Agent.newWithApiKeys(setup: AgentSetup(model: "gpt-5-mini"), apiKeys: ["openai": "sk-test"])
	}

	func testSinkSubscriptionLifecycle() {
		let agent = Agent(setup: AgentSetup(model: "gpt-5-mini"))
		let typedSub = agent.subscribe(sink: CollectSink())
		let jsonSub = agent.subscribeJson(sink: CollectJsonSink())
		typedSub.unsubscribe()
		jsonSub.unsubscribe()
		typedSub.unsubscribe() // idempotent
	}

	func testToolAndHookRegistration() throws {
		let agent = Agent(setup: AgentSetup(model: "gpt-5-mini"))
		let tool = EchoTool()
		agent.setTools(tools: [tool])
		agent.addTool(tool: tool)
		try agent.setBeforeToolCallHook(hook: AllowAll())
	}

	func testSnapshotAndIdleState() {
		let agent = Agent(setup: AgentSetup(model: "gpt-5-mini", systemPrompt: "Hi"))
		XCTAssertFalse(agent.isStreaming())
		let snapshot = agent.snapshot()
		XCTAssertEqual(snapshot.systemPrompt, "Hi")
		XCTAssertFalse(snapshot.isStreaming)
		XCTAssertTrue(snapshot.messages.isEmpty)
	}

	func testCancelTokenRoundTrip() {
		let agent = Agent(setup: AgentSetup(model: "gpt-5-mini"))
		XCTAssertNil(agent.signal()) // no run in flight
		agent.abort() // no-op without a run
	}
}
