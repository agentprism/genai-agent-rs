//! UniFFI (Swift) bindings for `rust-genai-agent` — Layer B of docs/embedding.md.
//!
//! Surface: construct from a data-only `AgentSetup` (with env or explicit
//! API-key auth), typed events via the `AgentEventSink` callback interface
//! (plus the JSON wire-format sink), host-implemented tools and loop hooks as
//! callback interfaces, prompt/continue, abort, and an explicit cancellation
//! token. The tokio runtime is crate-owned, lazy, and idempotent; host
//! callbacks fire on its worker threads and are awaited sequentially per
//! event (docs/embedding.md §8).
//!
//! Deliberately out of scope: host-side `StreamFn` (providers go through
//! `GenaiStreamFn`), steering/follow-up queue sources (they register via
//! consuming builder methods, which don't fit a shared FFI `Agent`),
//! `ConvertToLlm` (hosts keep the default), and the tool `UpdateSink`
//! (streaming partial tool results) — all additive later without breaking
//! this surface.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use rust_genai_agent as agent;

mod types;
pub use types::*;

uniffi::setup_scaffolding!();

/// The crate-owned tokio runtime (§8: lazy + idempotent, capped worker
/// count). UniFFI lets the host drive async futures, but the agent's
/// reqwest/hyper I/O needs a live tokio reactor, so real work is spawned here
/// and only the JoinHandle is awaited by the host's executor.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		// Capped rather than num-cores: these threads live for the process
		// lifetime on mobile.
		.worker_threads(4)
		.build()
		.expect("genai-agent-ffi: failed to build tokio runtime")
});

// region:    --- Event sinks

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

// region:    --- Host-implemented tools & hooks

/// A tool implemented by the host. `execute` fires on a runtime worker
/// thread; `cancel` is the run's cancellation token — long-running tools
/// should observe it. Throwing maps to a tool execution error.
///
/// NOTE: `execution_mode` (per-tool batching override) and the streaming
/// `UpdateSink` are not exposed yet; tools inherit the loop's mode.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
	fn spec(&self) -> ToolSpec;
	async fn execute(&self, call: ToolCallContext, cancel: Arc<AgentCancelToken>) -> Result<AgentToolResult, AgentError>;
}

struct ToolAdapter(Arc<dyn AgentTool>);

#[async_trait::async_trait]
impl agent::AgentTool for ToolAdapter {
	fn spec(&self) -> agent::ToolSpec {
		self.0.spec().into()
	}

	async fn execute(
		&self,
		call: agent::ToolCallContext,
		cancel: agent::CancellationToken,
		on_update: agent::UpdateSink,
	) -> Result<agent::AgentToolResult, agent::ToolError> {
		let _ = on_update; // not yet exposed to hosts (see module docs)
		self.0
			.execute(call.into(), Arc::new(AgentCancelToken { inner: cancel }))
			.await
			.map(Into::into)
			.map_err(|err| agent::ToolError::Execution(err.to_string()))
	}
}

/// Rewrites/transforms the transcript before each provider call.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait TransformContextHook: Send + Sync {
	async fn transform(&self, messages: Vec<AgentMessage>, cancel: Arc<AgentCancelToken>) -> Vec<AgentMessage>;
}

struct TransformContextAdapter(Arc<dyn TransformContextHook>);

#[async_trait::async_trait]
impl agent::TransformContext for TransformContextAdapter {
	async fn transform(
		&self,
		messages: Vec<agent::AgentMessage>,
		cancel: agent::CancellationToken,
	) -> Vec<agent::AgentMessage> {
		self.0
			.transform(
				messages.into_iter().map(Into::into).collect(),
				Arc::new(AgentCancelToken { inner: cancel }),
			)
			.await
			.into_iter()
			.map(Into::into)
			.collect()
	}
}

/// Gate/rewrite hook fired before each tool call. Return an outcome with
/// `decision.block = true` to deny, or `args_json` to rewrite arguments.
/// Malformed `args_json` blocks the call (safe failure) with the parse error
/// as the reason.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait BeforeToolCallHook: Send + Sync {
	async fn before(&self, ctx: BeforeToolCallContext, cancel: Arc<AgentCancelToken>) -> BeforeToolCallOutcome;
}

struct BeforeToolCallAdapter(Arc<dyn BeforeToolCallHook>);

#[async_trait::async_trait]
impl agent::BeforeToolCall for BeforeToolCallAdapter {
	async fn before(
		&self,
		ctx: agent::BeforeToolCallContext,
		cancel: agent::CancellationToken,
	) -> agent::BeforeToolCallOutcome {
		let outcome = self.0.before(ctx.into(), Arc::new(AgentCancelToken { inner: cancel })).await;
		let args = match outcome.args_json {
			Some(json) => match serde_json::from_str(&json) {
				Ok(value) => Some(value),
				Err(err) => {
					return agent::BeforeToolCallOutcome {
						args: None,
						decision: Some(agent::BeforeToolCallResult {
							block: true,
							reason: Some(format!("invalid args_json from host hook: {err}")),
							terminate: false,
						}),
					};
				}
			},
			None => None,
		};
		agent::BeforeToolCallOutcome {
			args,
			decision: outcome.decision.map(Into::into),
		}
	}
}

/// Observer/rewriter fired after each tool call.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait AfterToolCallHook: Send + Sync {
	async fn after(&self, ctx: AfterToolCallContext, cancel: Arc<AgentCancelToken>) -> Option<AfterToolCallResult>;
}

struct AfterToolCallAdapter(Arc<dyn AfterToolCallHook>);

#[async_trait::async_trait]
impl agent::AfterToolCall for AfterToolCallAdapter {
	async fn after(
		&self,
		ctx: agent::AfterToolCallContext,
		cancel: agent::CancellationToken,
	) -> Option<agent::AfterToolCallResult> {
		let result = self.0.after(ctx.into(), Arc::new(AgentCancelToken { inner: cancel })).await?;
		// An unparseable details_json keeps the original result (None = no override).
		let details = match result.details_json {
			Some(json) => match serde_json::from_str(&json) {
				Ok(value) => Some(value),
				Err(_) => None,
			},
			None => None,
		};
		Some(agent::AfterToolCallResult {
			content: result.content.map(|parts| parts.into_iter().map(Into::into).collect()),
			details,
			is_error: result.is_error,
			usage: result.usage.map(Into::into),
			terminate: result.terminate,
		})
	}
}

/// Fallible variant of `BeforeToolCallHook` — a thrown error aborts the call.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait TryBeforeToolCallHook: Send + Sync {
	async fn before(
		&self,
		ctx: BeforeToolCallContext,
		cancel: Arc<AgentCancelToken>,
	) -> Result<BeforeToolCallOutcome, AgentError>;
}

struct TryBeforeToolCallAdapter(Arc<dyn TryBeforeToolCallHook>);

#[async_trait::async_trait]
impl agent::TryBeforeToolCall for TryBeforeToolCallAdapter {
	async fn before(
		&self,
		ctx: agent::BeforeToolCallContext,
		cancel: agent::CancellationToken,
	) -> Result<agent::BeforeToolCallOutcome, agent::ToolHookError> {
		let outcome = self
			.0
			.before(ctx.into(), Arc::new(AgentCancelToken { inner: cancel }))
			.await
			.map_err(|err| agent::ToolHookError::new(err.to_string()))?;
		let args = match outcome.args_json {
			Some(json) => Some(serde_json::from_str(&json).map_err(|err| {
				agent::ToolHookError::new(format!("invalid args_json from host hook: {err}"))
			})?),
			None => None,
		};
		Ok(agent::BeforeToolCallOutcome {
			args,
			decision: outcome.decision.map(Into::into),
		})
	}
}

/// Fallible variant of `AfterToolCallHook`.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait TryAfterToolCallHook: Send + Sync {
	async fn after(
		&self,
		ctx: AfterToolCallContext,
		cancel: Arc<AgentCancelToken>,
	) -> Result<Option<AfterToolCallResult>, AgentError>;
}

struct TryAfterToolCallAdapter(Arc<dyn TryAfterToolCallHook>);

#[async_trait::async_trait]
impl agent::TryAfterToolCall for TryAfterToolCallAdapter {
	async fn after(
		&self,
		ctx: agent::AfterToolCallContext,
		cancel: agent::CancellationToken,
	) -> Result<Option<agent::AfterToolCallResult>, agent::ToolHookError> {
		let result = self
			.0
			.after(ctx.into(), Arc::new(AgentCancelToken { inner: cancel }))
			.await
			.map_err(|err| agent::ToolHookError::new(err.to_string()))?;
		match result {
			Some(result) => {
				let details = match result.details_json {
					Some(json) => Some(serde_json::from_str(&json).map_err(|err| {
						agent::ToolHookError::new(format!("invalid details_json from host hook: {err}"))
					})?),
					None => None,
				};
				Ok(Some(agent::AfterToolCallResult {
					content: result.content.map(|parts| parts.into_iter().map(Into::into).collect()),
					details,
					is_error: result.is_error,
					usage: result.usage.map(Into::into),
					terminate: result.terminate,
				}))
			}
			None => Ok(None),
		}
	}
}

/// A source of queued messages — steering (mid-run) or follow-up (post-run).
/// Polled by the loop; return an empty vec when nothing is queued.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait QueueMessageSource: Send + Sync {
	async fn poll(&self) -> Vec<AgentMessage>;
}

struct QueueSourceAdapter(Arc<dyn QueueMessageSource>);

#[async_trait::async_trait]
impl agent::QueueSource for QueueSourceAdapter {
	async fn poll(&self) -> Vec<agent::AgentMessage> {
		self.0.poll().await.into_iter().map(Into::into).collect()
	}
}

/// Decides whether the loop stops after a turn.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait ShouldStopAfterTurnHook: Send + Sync {
	async fn should_stop(&self, ctx: TurnContext, cancel: Arc<AgentCancelToken>) -> bool;
}

struct ShouldStopAdapter(Arc<dyn ShouldStopAfterTurnHook>);

#[async_trait::async_trait]
impl agent::ShouldStopAfterTurn for ShouldStopAdapter {
	async fn should_stop(&self, ctx: agent::ShouldStopAfterTurnContext, cancel: agent::CancellationToken) -> bool {
		self.0.should_stop(ctx.into(), Arc::new(AgentCancelToken { inner: cancel })).await
	}
}

/// Rewrites the next turn's context/model/thinking before it starts.
/// `AgentContextData.tools` is a read-only projection — a returned context
/// keeps the running configuration's tool objects.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait PrepareNextTurnHook: Send + Sync {
	async fn prepare(&self, ctx: TurnContext, cancel: Arc<AgentCancelToken>) -> Option<AgentLoopTurnUpdate>;
}

struct PrepareNextTurnAdapter(Arc<dyn PrepareNextTurnHook>);

#[async_trait::async_trait]
impl agent::PrepareNextTurn for PrepareNextTurnAdapter {
	async fn prepare(
		&self,
		ctx: agent::ShouldStopAfterTurnContext,
		cancel: agent::CancellationToken,
	) -> Option<agent::AgentLoopTurnUpdate> {
		let original_tools = ctx.context.tools.clone();
		let update = self.0.prepare(ctx.into(), Arc::new(AgentCancelToken { inner: cancel })).await?;
		Some(agent::AgentLoopTurnUpdate {
			context: update.context.map(|context| context.into_core(original_tools)),
			model: update.model.map(Into::into),
			thinking_level: update.thinking_level.map(Into::into),
		})
	}
}

// endregion: --- Host-implemented tools & hooks

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

/// An explicit cancellation handle for the in-flight run (or a tool/hook).
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
	/// Serializes tool-set read-modify-write (`add_tool`).
	tools_lock: Mutex<()>,
}

#[uniffi::export]
impl Agent {
	/// Build an agent from a data-only setup, using the default stream
	/// function (genai providers) with environment-resolved auth
	/// (OPENAI_API_KEY, …). Suited to CLI/tests; on device prefer
	/// `new_with_api_keys`.
	#[uniffi::constructor]
	pub fn new(setup: AgentSetup) -> Arc<Self> {
		Arc::new(Self {
			inner: agent::AgentBuilder::new(setup.into()).build(),
			tools_lock: Mutex::new(()),
		})
	}

	/// Build an agent with provider API keys injected by the host.
	///
	/// `api_keys` maps lowercased adapter-kind names to keys ("openai",
	/// "anthropic", "gemini", "xai", "groq", "deepseek", …). Providers without
	/// an entry fall back to genai's default (env-based) resolution.
	#[uniffi::constructor]
	pub fn new_with_api_keys(setup: AgentSetup, api_keys: HashMap<String, String>) -> Arc<Self> {
		let auth_resolver = genai::resolver::AuthResolver::from_resolver_fn(move |model_iden: genai::ModelIden| {
			let kind = model_iden.adapter_kind.to_string().to_lowercase();
			Ok(api_keys.get(&kind).map(|key| genai::resolver::AuthData::from_single(key.clone())))
		});
		let client = genai::Client::builder().with_auth_resolver(auth_resolver).build();
		let stream_fn = genai::stream_fn::GenaiStreamFn::new(client);
		Arc::new(Self {
			inner: agent::AgentBuilder::new(setup.into()).stream_fn(Arc::new(stream_fn)).build(),
			tools_lock: Mutex::new(()),
		})
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
		self.inner.signal().map(|inner| Arc::new(AgentCancelToken { inner }))
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

	// -- Tools & hooks

	/// Replace the agent's tool set with host-implemented tools.
	pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
		let adapted: Vec<Arc<dyn agent::AgentTool>> = tools
			.into_iter()
			.map(|tool| Arc::new(ToolAdapter(tool)) as Arc<dyn agent::AgentTool>)
			.collect();
		self.inner.set_tools(adapted);
	}

	/// Register one host-implemented tool (keeps existing tools).
	pub fn add_tool(&self, tool: Arc<dyn AgentTool>) {
		let _guard = self.tools_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let mut tools = self.inner.state().tools.clone();
		tools.push(Arc::new(ToolAdapter(tool)) as Arc<dyn agent::AgentTool>);
		self.inner.set_tools(tools);
	}

	/// Set (or clear with `None`) the transcript-transform hook.
	pub fn set_transform_context_hook(&self, hook: Arc<dyn TransformContextHook>) -> Result<(), AgentError> {
		self.inner
			.set_transform_context_object(Arc::new(TransformContextAdapter(hook)) as Arc<dyn agent::TransformContext>)
			.map_err(AgentError::from)
	}

	/// Set (or clear) the before-tool-call gate hook.
	pub fn set_before_tool_call_hook(&self, hook: Arc<dyn BeforeToolCallHook>) -> Result<(), AgentError> {
		self.inner
			.set_before_tool_call_object(Arc::new(BeforeToolCallAdapter(hook)) as Arc<dyn agent::BeforeToolCall>)
			.map_err(AgentError::from)
	}

	/// Set (or clear) the after-tool-call observer hook.
	pub fn set_after_tool_call_hook(&self, hook: Arc<dyn AfterToolCallHook>) -> Result<(), AgentError> {
		self.inner
			.set_after_tool_call_object(Arc::new(AfterToolCallAdapter(hook)) as Arc<dyn agent::AfterToolCall>)
			.map_err(AgentError::from)
	}

	/// Set (or clear) the fallible before-tool-call hook.
	pub fn set_try_before_tool_call_hook(&self, hook: Arc<dyn TryBeforeToolCallHook>) -> Result<(), AgentError> {
		self.inner
			.set_try_before_tool_call_object(Arc::new(TryBeforeToolCallAdapter(hook)) as Arc<dyn agent::TryBeforeToolCall>)
			.map_err(AgentError::from)
	}

	/// Set (or clear) the fallible after-tool-call hook.
	pub fn set_try_after_tool_call_hook(&self, hook: Arc<dyn TryAfterToolCallHook>) -> Result<(), AgentError> {
		self.inner
			.set_try_after_tool_call_object(Arc::new(TryAfterToolCallAdapter(hook)) as Arc<dyn agent::TryAfterToolCall>)
			.map_err(AgentError::from)
	}

	/// Set (or clear) the should-stop-after-turn hook.
	pub fn set_should_stop_after_turn_hook(&self, hook: Arc<dyn ShouldStopAfterTurnHook>) -> Result<(), AgentError> {
		self.inner
			.set_should_stop_after_turn_object(Arc::new(ShouldStopAdapter(hook)) as Arc<dyn agent::ShouldStopAfterTurn>)
			.map_err(AgentError::from)
	}

	/// Set (or clear) the prepare-next-turn hook.
	pub fn set_prepare_next_turn_hook(&self, hook: Arc<dyn PrepareNextTurnHook>) -> Result<(), AgentError> {
		self.inner
			.set_prepare_next_turn_object(Arc::new(PrepareNextTurnAdapter(hook)) as Arc<dyn agent::PrepareNextTurn>)
			.map_err(AgentError::from)
	}

	/// Set (or clear with `None`) the steering message source (polled mid-run).
	pub fn set_steering_source(&self, source: Option<Arc<dyn QueueMessageSource>>) -> Result<(), AgentError> {
		self.inner
			.set_steering_source_object(source.map(|s| Arc::new(QueueSourceAdapter(s)) as Arc<dyn agent::QueueSource>))
			.map_err(AgentError::from)
	}

	/// Set (or clear with `None`) the follow-up message source (polled between runs).
	pub fn set_follow_up_source(&self, source: Option<Arc<dyn QueueMessageSource>>) -> Result<(), AgentError> {
		self.inner
			.set_follow_up_source_object(source.map(|s| Arc::new(QueueSourceAdapter(s)) as Arc<dyn agent::QueueSource>))
			.map_err(AgentError::from)
	}
}

// Not exported over FFI: test-only construction with an injected stream function.
#[cfg(all(test, feature = "testing"))]
impl Agent {
	pub(crate) fn new_with_stream_fn(setup: AgentSetup, stream_fn: Arc<dyn agent::StreamFn>) -> Arc<Self> {
		Arc::new(Self {
			inner: agent::AgentBuilder::new(setup.into()).stream_fn(stream_fn).build(),
			tools_lock: Mutex::new(()),
		})
	}
}

// endregion: --- Agent

// region:    --- Tests

#[cfg(all(test, feature = "testing"))]
mod tests {
	use super::*;
	use agent::testing::{MockStreamFn, fixtures};
	use std::sync::atomic::{AtomicBool, Ordering};

	struct CollectSink(Mutex<Vec<AgentEvent>>);

	#[async_trait::async_trait]
	impl AgentEventSink for CollectSink {
		async fn emit(&self, event: AgentEvent) {
			self.0.lock().unwrap().push(event);
		}
	}

	fn no_chat_options() -> ChatOptions {
		ChatOptions {
			temperature: None,
			max_tokens: None,
			top_p: None,
			stop_sequences: vec![],
			capture_usage: None,
			capture_content: None,
			capture_reasoning_content: None,
			capture_tool_calls: None,
			capture_raw_body: None,
			response_format: None,
			tool_choice: None,
			normalize_reasoning_content: None,
			reasoning_effort: None,
			verbosity: None,
			seed: None,
			service_tier: None,
			extra_headers: None,
			cache_control: None,
			prompt_cache_key: None,
			extra_body_json: None,
		}
	}

	fn test_setup() -> AgentSetup {
		AgentSetup {
			system_prompt: String::new(),
			model: "mock".to_string(),
			session_id: None,
			messages: vec![],
			thinking_level: ThinkingLevel::Off,
			thinking_budgets: None,
			max_retries: None,
			max_retry_delay_ms: None,
			tool_execution: ToolExecutionMode::Parallel,
			transport: Transport::Auto,
			steering_mode: QueueMode::All,
			follow_up_mode: QueueMode::All,
			chat_options: no_chat_options(),
		}
	}

	#[tokio::test]
	async fn prompt_drives_typed_sink_events() {
		let mock = MockStreamFn::from_messages(vec![fixtures::text_msg("hello from rust")]);
		let agent = Agent::new_with_stream_fn(test_setup(), Arc::new(mock));

		let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
		let subscription = agent.subscribe(sink.clone());

		agent.prompt("hi".to_string()).await.unwrap();
		assert!(!agent.is_streaming());

		let events = sink.0.lock().unwrap();
		assert!(matches!(events.first(), Some(AgentEvent::AgentStart)), "first: {:?}", events.first());
		assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })), "last: {:?}", events.last());
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
		let agent = Agent::new_with_stream_fn(test_setup(), Arc::new(mock));

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
		for raw in events.iter() {
			let _: serde_json::Value = serde_json::from_str(raw).expect("event JSON parses");
		}
		assert!(events.first().unwrap().contains("AgentStart"));
	}

	#[test]
	fn api_keys_constructor_builds() {
		let keys: HashMap<String, String> = [("openai".to_string(), "sk-test".to_string())].into_iter().collect();
		let _ = Agent::new_with_api_keys(test_setup(), keys);
	}

	// -- B2: tools & hooks through the FFI boundary

	struct WeatherTool {
		executed: AtomicBool,
	}

	#[async_trait::async_trait]
	impl AgentTool for WeatherTool {
		fn spec(&self) -> ToolSpec {
			ToolSpec {
				name: "get_weather".to_string(),
				label: "Get Weather".to_string(),
				description: "Get the weather for a city".to_string(),
				schema_json: r#"{"type":"object","properties":{"city":{"type":"string"}}}"#.to_string(),
				strict: None,
			}
		}

		async fn execute(&self, call: ToolCallContext, _cancel: Arc<AgentCancelToken>) -> Result<AgentToolResult, AgentError> {
			self.executed.store(true, Ordering::SeqCst);
			assert_eq!(call.tool_name, "get_weather");
			assert!(call.args_json.contains("Lisbon"), "args_json: {}", call.args_json);
			Ok(AgentToolResult {
				content: vec![ToolResultContent::Text {
					text: "sunny, 24°C".to_string(),
				}],
				details_json: "{}".to_string(),
				usage: None,
				added_tool_names: vec![],
				terminate: false,
			})
		}
	}

	fn tool_call_setup() -> (AgentSetup, Arc<MockStreamFn>) {
		let tool_call = agent::AgentToolCall::new("call-1", "get_weather", serde_json::json!({ "city": "Lisbon" }));
		let mock = Arc::new(MockStreamFn::from_messages(vec![
			fixtures::tool_use_msg(vec![tool_call]),
			fixtures::text_msg("It is sunny in Lisbon."),
		]));
		(test_setup(), mock)
	}

	#[tokio::test]
	async fn host_tool_executes_and_streams_events() {
		let (setup, mock) = tool_call_setup();
		let tool = Arc::new(WeatherTool {
			executed: AtomicBool::new(false),
		});
		let agent = Agent::new_with_stream_fn(setup, mock);
		agent.set_tools(vec![tool.clone()]);

		let sink = Arc::new(CollectSink(Mutex::new(Vec::new())));
		let _sub = agent.subscribe(sink.clone());

		agent.prompt("weather in Lisbon?".to_string()).await.unwrap();

		assert!(tool.executed.load(Ordering::SeqCst), "host tool must execute");
		let events = sink.0.lock().unwrap();
		let saw_tool_end = events.iter().any(|e| {
			matches!(
				e,
				AgentEvent::ToolExecutionEnd { tool_name, result, is_error: false, .. }
					if tool_name == "get_weather"
						&& result.content.iter().any(|c| matches!(c, ToolResultContent::Text { text } if text.contains("sunny")))
			)
		});
		assert!(saw_tool_end, "expected ToolExecutionEnd with the host tool's result");
	}

	#[tokio::test]
	async fn steering_source_injects_message_into_run() {
		let mock = MockStreamFn::from_messages(vec![
			fixtures::text_msg("first answer"),
			fixtures::text_msg("answer after steering"),
		]);
		let agent = Agent::new_with_stream_fn(test_setup(), Arc::new(mock));

		struct SteerOnce;
		#[async_trait::async_trait]
		impl QueueMessageSource for SteerOnce {
			async fn poll(&self) -> Vec<AgentMessage> {
				vec![AgentMessage::User(UserMessage {
					content: vec![UserContent::Text {
						text: "steer me".to_string(),
					}],
					timestamp: 0,
				})]
			}
		}

		agent
			.set_steering_source(Some(Arc::new(SteerOnce)))
			.expect("source registration while idle");
		agent.prompt("start".to_string()).await.unwrap();

		let snapshot = agent.snapshot();
		let transcript_has = |needle: &str| {
			snapshot.messages.iter().any(|m| {
				matches!(m, AgentMessage::User(u) if u.content.iter().any(|c| matches!(c, UserContent::Text { text } if text.contains(needle))))
					|| matches!(m, AgentMessage::Assistant(a) if a.content.iter().any(|c| matches!(c, AssistantContent::Text { text, .. } if text.contains(needle))))
			})
		};
		assert!(transcript_has("steer me"), "steered message should be in the transcript");
		assert!(transcript_has("answer after steering"), "loop should continue after steering");
	}

	#[tokio::test]
	async fn before_tool_call_hook_blocks_execution() {
		let (setup, mock) = tool_call_setup();
		let tool = Arc::new(WeatherTool {
			executed: AtomicBool::new(false),
		});
		let agent = Agent::new_with_stream_fn(setup, mock);
		agent.set_tools(vec![tool.clone()]);

		struct DenyAll;
		#[async_trait::async_trait]
		impl BeforeToolCallHook for DenyAll {
			async fn before(&self, ctx: BeforeToolCallContext, _cancel: Arc<AgentCancelToken>) -> BeforeToolCallOutcome {
				assert_eq!(ctx.tool_call.name, "get_weather");
				BeforeToolCallOutcome {
					args_json: None,
					decision: Some(BeforeToolCallResult {
						block: true,
						reason: Some("denied by host".to_string()),
						terminate: false,
					}),
				}
			}
		}

		agent
			.set_before_tool_call_hook(Arc::new(DenyAll))
			.expect("hook registration while idle");
		agent.prompt("weather in Lisbon?".to_string()).await.unwrap();

		assert!(
			!tool.executed.load(Ordering::SeqCst),
			"blocked tool must not execute"
		);
	}
}

// endregion: --- Tests
