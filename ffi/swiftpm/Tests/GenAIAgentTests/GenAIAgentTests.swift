import XCTest
@testable import GenAIAgent

/// Offline tests: exercise the FFI surface (construction, sink registration,
/// cancellation token) without network access. The full loop is exercised
/// Rust-side by `cargo test -p genai-agent-ffi --features testing`, which
/// drives a scripted provider stream through these same objects.
final class GenAIAgentTests: XCTestCase {

	// Callbacks fire on tokio worker threads, so the sinks guard their state.
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

	func testAgentConstructsFromSetup() throws {
		let setup = AgentSetup(model: "gpt-5-mini", systemPrompt: "You are concise")
		_ = try Agent(setup: setup)
	}

	func testMalformedInitialMessagesJsonThrows() {
		let setup = AgentSetup(model: "gpt-5-mini", initialMessagesJson: "not json")
		XCTAssertThrowsError(try Agent(setup: setup)) { error in
			guard case AgentError.Other(let message) = error else {
				return XCTFail("expected AgentError.Other, got \(error)")
			}
			XCTAssertTrue(message.contains("initial_messages_json"), "unexpected message: \(message)")
		}
	}

	func testSinkSubscriptionLifecycle() throws {
		let agent = try Agent(setup: AgentSetup(model: "gpt-5-mini"))
		let typed = CollectSink()
		let json = CollectJsonSink()
		let typedSub = agent.subscribe(sink: typed)
		let jsonSub = agent.subscribeJson(sink: json)
		typedSub.unsubscribe()
		jsonSub.unsubscribe()
		// Idempotent unsubscribe is safe.
		typedSub.unsubscribe()
	}

	func testSnapshotAndIdleState() throws {
		let agent = try Agent(setup: AgentSetup(model: "gpt-5-mini", systemPrompt: "Hi"))
		XCTAssertFalse(agent.isStreaming())
		let snapshot = agent.snapshot()
		XCTAssertEqual(snapshot.systemPrompt, "Hi")
		XCTAssertFalse(snapshot.isStreaming)
		XCTAssertTrue(snapshot.messages.isEmpty)
	}

	func testCancelTokenRoundTrip() throws {
		let agent = try Agent(setup: AgentSetup(model: "gpt-5-mini"))
		XCTAssertNil(agent.signal()) // no run in flight
		agent.abort() // no-op without a run
	}
}
