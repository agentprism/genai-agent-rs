//! UniFFI (Swift) bindings for `rust-genai-agent` — Layer B1 of docs/embedding.md.
//!
//! B1 surface: construct from `AgentSetup`, subscribe one typed `EventSink`
//! (plus the JSON wire-format sink), `prompt` / `continueRun`, `abort`, and an
//! explicit cancellation token. The tokio runtime is crate-owned, lazy, and
//! idempotent; host callbacks fire on its worker threads and are awaited
//! sequentially per event (see docs/embedding.md §8).
//!
//! Out of scope for B1 (see §7): host-implemented tools and hooks (B2),
//! host-side `StreamFn`, typed `chat_options`, typed initial messages.

use std::sync::{Arc, LazyLock, Mutex};

use rust_genai_agent as agent;

mod types;
pub use types::*;

uniffi::setup_scaffolding!();

/// The crate-owned tokio runtime (docs/embedding.md §8: lazy + idempotent,
/// capped worker count). UniFFI lets the host drive async futures, but the
/// agent's reqwest/hyper I/O needs a live tokio reactor, so real work is
/// spawned here and only the JoinHandle is awaited by the host's executor.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		// Capped rather than num-cores: these threads live for the process
		// lifetime on mobile.
		.worker_threads(4)
		.build()
		.expect("genai-agent-ffi: failed to build tokio runtime")
});

// region:    --- Event sinks (host-implemented callback interfaces)

/// The typed event path — the contract (§8). The host implements this in
/// Swift; `emit` is awaited sequentially per event (a slow sink applies
/// backpressure) and fires on runtime worker threads (hop to the main actor
/// there if updating UI).
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait AgentEventSink: Send + Sync {
	async fn emit(&self, event: AgentEvent);
}

/// The wire-format proof sink: each event as its serde JSON string.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait AgentEventJsonSink: Send + Sync {
	async fn emit_json(&self, event_json: String);
}

struct SinkAdapter(Arc<dyn AgentEventSink>);

#[async_trait::async_trait]
impl agent::EventSink for SinkAdapter {
	async fn emit(&self, event: agent::AgentEvent) {
		self.0.emit(event.into()).await;
	}
}

struct JsonSinkAdapter(Arc<dyn AgentEventJsonSink>);

#[async_trait::async_trait]
impl agent::EventSink for JsonSinkAdapter {
	async fn emit(&self, event: agent::AgentEvent) {
		match serde_json::to_string(&event) {
			Ok(json) => self.0.emit_json(json).await,
			Err(err) => debug_assert!(false, "AgentEvent serialization failed: {err}"),
		}
	}
}

// endregion: --- Event sinks

// region:    --- Subscription / cancellation

/// A sink registration. Dropping unsubscribes; `unsubscribe()` is explicit.
#[derive(uniffi::Object)]
pub struct Subscription {
	inner: Mutex<Option<agent::Subscription>>,
}

impl Subscription {
	fn new(subscription: agent::Subscription) -> Arc<Self> {
		Arc::new(Self {
			inner: Mutex::new(Some(subscription)),
		})
	}
}

#[uniffi::export]
impl Subscription {
	pub fn unsubscribe(&self) {
		if let Ok(mut guard) = self.inner.lock() {
			if let Some(subscription) = guard.take() {
				subscription.unsubscribe();
			}
		}
	}
}

/// An explicit cancellation handle for the in-flight run.
#[derive(uniffi::Object)]
pub struct AgentCancelToken {
	inner: agent::CancellationToken,
}

#[uniffi::export]
impl AgentCancelToken {
	pub fn cancel(&self) {
		self.inner.cancel();
	}

	pub fn is_cancelled(&self) -> bool {
		self.inner.is_cancelled()
	}
}

// endregion: --- Subscription / cancellation

// region:    --- Agent

/// A `rust_genai_agent::Agent` exposed to the host.
#[derive(uniffi::Object)]
pub struct Agent {
	inner: agent::Agent,
}

#[uniffi::export]
impl Agent {
	/// Build an agent from a data-only setup. Uses the crate's default stream
	/// function (genai providers, env-configured auth).
	#[uniffi::constructor]
	pub fn new(setup: AgentSetup) -> Result<Arc<Self>, AgentError> {
		Ok(Arc::new(Self {
			inner: agent::AgentBuilder::new(setup.try_into_core()?).build(),
		}))
	}

	/// Send a user text prompt and drive the run to completion. Events flow to
	/// subscribed sinks while this awaits.
	pub async fn prompt(&self, text: String) -> Result<(), AgentError> {
		let agent = self.inner.clone();
		RUNTIME
			.spawn(async move { agent.prompt(text).await.map_err(AgentError::from) })
			.await
			.map_err(join_err)?
	}

	/// Continue the loop from the current transcript (core `continue_`).
	pub async fn continue_run(&self) -> Result<(), AgentError> {
		let agent = self.inner.clone();
		RUNTIME
			.spawn(async move { agent.continue_().await.map_err(AgentError::from) })
			.await
			.map_err(join_err)?
	}

	/// Wait until no run is in flight.
	pub async fn wait_for_idle(&self) {
		let agent = self.inner.clone();
		let _ = RUNTIME.spawn(async move { agent.wait_for_idle().await }).await;
	}

	/// The cancellation token for the in-flight run, if one is running.
	pub fn signal(&self) -> Option<Arc<AgentCancelToken>> {
		self.inner
			.signal()
			.map(|inner| Arc::new(AgentCancelToken { inner }))
	}

	/// Cancel the in-flight run, if any.
	pub fn abort(&self) {
		self.inner.abort();
	}

	/// Whether a run is currently streaming.
	pub fn is_streaming(&self) -> bool {
		self.inner.state().is_streaming
	}

	/// A render-friendly snapshot of the agent state (see `AgentSnapshot`).
	pub fn snapshot(&self) -> AgentSnapshot {
		self.inner.state().into()
	}

	/// Subscribe the typed event sink. Drop/unsubscribe the returned
	/// `Subscription` to stop receiving events.
	pub fn subscribe(&self, sink: Arc<dyn AgentEventSink>) -> Arc<Subscription> {
		Subscription::new(self.inner.subscribe_sink(Arc::new(SinkAdapter(sink))))
	}

	/// Subscribe the JSON wire-format sink (events as serde JSON strings).
	pub fn subscribe_json(&self, sink: Arc<dyn AgentEventJsonSink>) -> Arc<Subscription> {
		Subscription::new(self.inner.subscribe_sink(Arc::new(JsonSinkAdapter(sink))))
	}
}

// Not exported over FFI: test-only construction with an injected stream function.
#[cfg(feature = "testing")]
impl Agent {
	pub(crate) fn new_with_stream_fn(
		setup: AgentSetup,
		stream_fn: Arc<dyn agent::StreamFn>,
	) -> Result<Arc<Self>, AgentError> {
		Ok(Arc::new(Self {
			inner: agent::AgentBuilder::new(setup.try_into_core()?).stream_fn(stream_fn).build(),
		}))
	}
}

// endregion: --- Agent

// region:    --- Tests

#[cfg(all(test, feature = "testing"))]
mod tests {
	use super::*;
	use agent::testing::{MockStreamFn, fixtures};

	/// A Rust implementation of the host-facing sink, collecting events.
	struct CollectSink(Mutex<Vec<AgentEvent>>);

	#[async_trait::async_trait]
	impl AgentEventSink for CollectSink {
		async fn emit(&self, event: AgentEvent) {
			self.0.lock().unwrap().push(event);
		}
	}

	fn test_setup() -> AgentSetup {
		AgentSetup {
			system_prompt: String::new(),
			model: "mock".to_string(),
			session_id: None,
			thinking_level: ThinkingLevel::Off,
			thinking_budgets: None,
			max_retries: None,
			max_retry_delay_ms: None,
			tool_execution: ToolExecutionMode::Parallel,
			transport: Transport::Auto,
			steering_mode: QueueMode::All,
			follow_up_mode: QueueMode::All,
			initial_messages_json: None,
		}
	}

	#[tokio::test]
	async fn prompt_drives_typed_sink_events() {
		let mock = MockStreamFn::from_messages(vec![fixtures::text_msg("hello from rust")]);
		let agent = Agent::new_with_stream_fn(test_setup(), Arc::new(mock)).unwrap();

		let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
		let subscription = agent.subscribe(sink.clone());

		agent.prompt("hi".to_string()).await.unwrap();
		assert!(!agent.is_streaming());

		let events = sink.0.lock().unwrap();
		// Bookends
		assert!(matches!(events.first(), Some(AgentEvent::AgentStart)), "first: {:?}", events.first());
		assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })), "last: {:?}", events.last());
		// The scripted text arrives via MessageUpdate with a self-contained partial.
		let saw_text = events.iter().any(|e| {
			matches!(
				e,
				AgentEvent::MessageUpdate { message: AgentMessage::Assistant(m), .. }
					if m.content.iter().any(|c| matches!(c, AssistantContent::Text { text, .. } if text.contains("hello from rust")))
			)
		});
		assert!(saw_text, "expected a MessageUpdate carrying the scripted text; events: {:?}", events.len());
		drop(events);

		subscription.unsubscribe();
	}

	#[tokio::test]
	async fn json_sink_receives_wire_format() {
		let mock = MockStreamFn::from_messages(vec![fixtures::text_msg("wire")]);
		let agent = Agent::new_with_stream_fn(test_setup(), Arc::new(mock)).unwrap();

		struct JsonCollect(Mutex<Vec<String>>);
		#[async_trait::async_trait]
		impl AgentEventJsonSink for JsonCollect {
			async fn emit_json(&self, event_json: String) {
				self.0.lock().unwrap().push(event_json);
			}
		}

		let sink = Arc::new(JsonCollect(Mutex::new(Vec::new())));
		let _sub = agent.subscribe_json(sink.clone());
		agent.prompt("hi".to_string()).await.unwrap();

		let events = sink.0.lock().unwrap();
		assert!(!events.is_empty());
		// Every payload is valid JSON; the first is the AgentStart event.
		for raw in events.iter() {
			let _: serde_json::Value = serde_json::from_str(raw).expect("event JSON parses");
		}
		assert!(events.first().unwrap().contains("AgentStart"));
	}

	#[test]
	fn malformed_initial_messages_json_is_a_thrown_error() {
		let mut setup = test_setup();
		setup.initial_messages_json = Some("not json".to_string());
		let err = Agent::new(setup).err().expect("construction must fail");
		assert!(matches!(err, AgentError::Other(_)));
	}
}

// endregion: --- Tests
