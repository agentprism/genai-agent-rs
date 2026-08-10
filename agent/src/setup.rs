//! Declarative, data-only agent configuration ([`AgentSetup`]) and a builder ([`AgentBuilder`])
//! that assembles a live [`Agent`] from that data plus trait-object behavior.
//!
//! This is the additive, foreign-friendly construction path introduced in Layer A1 (see
//! `docs/embedding.md` §3.5). The split is deliberate:
//!
//! - **Configuration is data.** [`AgentSetup`] is a plain `serde`-able record — no closures, no
//!   `Arc<dyn …>`. A host (Swift/Kotlin via UniFFI, or a Rust caller) declares it directly, and it
//!   round-trips through JSON for persistence or transport.
//! - **Behavior is trait objects.** [`AgentBuilder`] carries an [`AgentSetup`] and optional
//!   `Arc<dyn Trait>` behavior (a [`StreamFn`], [`AgentTool`]s, and the loop hooks), attaching each
//!   through the *same* trait→closure adapters the runtime [`Agent::set_before_tool_call_object`]
//!   family uses, so a hook behaves identically no matter which entry point installs it.
//!
//! The existing [`AgentConfig`]/[`AgentState`] closure API is untouched; this module only adds a new
//! way in.

use crate::hooks::{
    after_tool_call_to_hook, before_tool_call_to_hook, message_converter_to_hook,
    prepare_next_turn_to_hook, should_stop_after_turn_to_hook, transform_context_to_hook,
    try_after_tool_call_to_hook, try_before_tool_call_to_hook,
};
use crate::{
    AfterToolCall, Agent, AgentConfig, AgentMessage, AgentState, AgentTool, BeforeToolCall,
    ChatOptions, MessageConverter, PrepareNextTurn, QueueMode, QueueSource, ShouldStopAfterTurn,
    StreamFn, ThinkingBudgets, ThinkingLevel, ToolExecutionMode, TransformContext, Transport,
    TryAfterToolCall, TryBeforeToolCall,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Declarative, `serde`-able snapshot of everything an agent is configured *with* — data only.
///
/// Every field is a plain value (no closures, no `Arc<dyn …>`), so an `AgentSetup` can be built
/// declaratively by a non-Rust host and round-tripped through JSON. Pair it with [`AgentBuilder`] to
/// attach behavior (stream function, tools, hooks) and produce a live [`Agent`].
///
/// The fields mirror the data-shaped subset of [`AgentState`] (system prompt, model, initial
/// transcript, thinking level) and [`AgentConfig`] (session id, thinking budgets, retry caps, tool
/// execution policy, transport, steering/follow-up drain modes, chat options). The `Option` fields
/// map one-to-one onto their `AgentConfig` counterparts and are applied only when `Some`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentSetup {
    /// System instruction captured into the built agent's initial state.
    pub system_prompt: String,
    /// Model name/slug. [`AgentBuilder::build`] converts it via `Into<ModelSpec>` (a bare name
    /// becomes `ModelSpec::Name`).
    ///
    /// Defaults to the empty string: the crate's own default model is an internal
    /// `ModelSpec::Iden` placeholder (`ollama::unknown`) that does not round-trip as a plain name,
    /// so there is no data-representable default slug to seed. A caller that leaves this empty must
    /// set a real model before a run reaches a live provider.
    pub model: String,
    /// Optional session identifier forwarded onto each stream request (pi's `sessionId`).
    ///
    /// Independent of `ChatOptions::prompt_cache_key`; applied via [`AgentConfig::with_session_id`]
    /// only when set.
    pub session_id: Option<String>,
    /// Initial transcript captured into the built agent's state.
    pub messages: Vec<AgentMessage>,
    /// Reasoning intensity captured into the built agent's state.
    pub thinking_level: ThinkingLevel,
    /// Optional per-named-level reasoning-token budgets used to resolve [`Self::thinking_level`].
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Optional maximum number of provider-handshake retries forwarded onto each stream request.
    pub max_retries: Option<u32>,
    /// Optional cap, in milliseconds, on a server-requested retry delay forwarded onto each stream
    /// request.
    pub max_retry_delay_ms: Option<u64>,
    /// Tool-call batch execution policy.
    pub tool_execution: ToolExecutionMode,
    /// Preferred provider transport advisory forwarded to the stream function.
    pub transport: Transport,
    /// Initial steering-queue drain policy.
    pub steering_mode: QueueMode,
    /// Initial follow-up-queue drain policy.
    pub follow_up_mode: QueueMode,
    /// Base provider chat options.
    pub chat_options: ChatOptions,
}

/// Builder that assembles a live [`Agent`] from an [`AgentSetup`] (data) plus optional trait-object
/// behavior.
///
/// Behavior is attached with `Arc<dyn Trait>` setters that mirror the runtime `set_*_object`
/// registrations; [`Self::build`] wires each into an [`AgentConfig`] using the same trait→closure
/// adapters, then calls [`Agent::new`]. A default `AgentSetup` with no behavior produces an agent
/// whose configuration equals `AgentConfig::default()` (aside from the data-derived initial state).
///
/// ```no_run
/// # use std::sync::Arc;
/// # use rust_genai_agent::{AgentBuilder, AgentSetup};
/// # fn demo(backend: Arc<dyn rust_genai_agent::StreamFn>, tool: Arc<dyn rust_genai_agent::AgentTool>) {
/// let agent = AgentBuilder::new(AgentSetup {
///     system_prompt: "You are helpful.".to_string(),
///     model: "gpt-5.6-sol".to_string(),
///     ..AgentSetup::default()
/// })
/// .stream_fn(backend)
/// .tool(tool)
/// .build();
/// # let _ = agent;
/// # }
/// ```
///
/// **Event sinks are intentionally not part of the builder.** Event subscription has its own RAII
/// [`crate::Subscription`] lifecycle, so the host attaches sinks *after* construction with
/// [`Agent::subscribe_sink`], retaining the returned subscription for as long as the sink should
/// stay registered.
#[derive(Default)]
pub struct AgentBuilder {
    setup: AgentSetup,
    stream_fn: Option<Arc<dyn StreamFn>>,
    tools: Vec<Arc<dyn AgentTool>>,
    convert_to_llm: Option<Arc<dyn MessageConverter>>,
    transform_context: Option<Arc<dyn TransformContext>>,
    before_tool_call: Option<Arc<dyn BeforeToolCall>>,
    after_tool_call: Option<Arc<dyn AfterToolCall>>,
    try_before_tool_call: Option<Arc<dyn TryBeforeToolCall>>,
    try_after_tool_call: Option<Arc<dyn TryAfterToolCall>>,
    should_stop_after_turn: Option<Arc<dyn ShouldStopAfterTurn>>,
    prepare_next_turn: Option<Arc<dyn PrepareNextTurn>>,
    steering_source: Option<Arc<dyn QueueSource>>,
    follow_up_source: Option<Arc<dyn QueueSource>>,
}

impl std::fmt::Debug for AgentBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentBuilder")
            .field("setup", &self.setup)
            .field("stream_fn", &self.stream_fn.is_some())
            .field("tools", &self.tools.len())
            .field("convert_to_llm", &self.convert_to_llm.is_some())
            .field("transform_context", &self.transform_context.is_some())
            .field("before_tool_call", &self.before_tool_call.is_some())
            .field("after_tool_call", &self.after_tool_call.is_some())
            .field("try_before_tool_call", &self.try_before_tool_call.is_some())
            .field("try_after_tool_call", &self.try_after_tool_call.is_some())
            .field(
                "should_stop_after_turn",
                &self.should_stop_after_turn.is_some(),
            )
            .field("prepare_next_turn", &self.prepare_next_turn.is_some())
            .field("steering_source", &self.steering_source.is_some())
            .field("follow_up_source", &self.follow_up_source.is_some())
            .finish()
    }
}

impl From<AgentSetup> for AgentBuilder {
    fn from(setup: AgentSetup) -> Self {
        Self::new(setup)
    }
}

impl AgentBuilder {
    /// Start a builder from a data-only [`AgentSetup`], with no behavior attached yet.
    pub fn new(setup: AgentSetup) -> Self {
        Self {
            setup,
            ..Self::default()
        }
    }

    /// Install the provider stream function (the model backend).
    pub fn stream_fn(mut self, stream_fn: Arc<dyn StreamFn>) -> Self {
        self.stream_fn = Some(stream_fn);
        self
    }

    /// Append one tool to the agent's tool set.
    pub fn tool(mut self, tool: Arc<dyn AgentTool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Append a batch of tools to the agent's tool set.
    pub fn tools(mut self, tools: impl IntoIterator<Item = Arc<dyn AgentTool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    /// Install an object-safe [`MessageConverter`] as the provider-boundary transcript converter.
    ///
    /// Most hosts keep the default converter and never call this.
    pub fn convert_to_llm(mut self, converter: Arc<dyn MessageConverter>) -> Self {
        self.convert_to_llm = Some(converter);
        self
    }

    /// Install an object-safe [`TransformContext`] provider-boundary transcript transform.
    pub fn transform_context(mut self, hook: Arc<dyn TransformContext>) -> Self {
        self.transform_context = Some(hook);
        self
    }

    /// Install an object-safe infallible [`BeforeToolCall`] hook.
    pub fn before_tool_call(mut self, hook: Arc<dyn BeforeToolCall>) -> Self {
        self.before_tool_call = Some(hook);
        self
    }

    /// Install an object-safe infallible [`AfterToolCall`] hook.
    pub fn after_tool_call(mut self, hook: Arc<dyn AfterToolCall>) -> Self {
        self.after_tool_call = Some(hook);
        self
    }

    /// Install an object-safe fallible [`TryBeforeToolCall`] hook (takes precedence over
    /// [`Self::before_tool_call`]).
    pub fn try_before_tool_call(mut self, hook: Arc<dyn TryBeforeToolCall>) -> Self {
        self.try_before_tool_call = Some(hook);
        self
    }

    /// Install an object-safe fallible [`TryAfterToolCall`] hook (takes precedence over
    /// [`Self::after_tool_call`]).
    pub fn try_after_tool_call(mut self, hook: Arc<dyn TryAfterToolCall>) -> Self {
        self.try_after_tool_call = Some(hook);
        self
    }

    /// Install an object-safe [`ShouldStopAfterTurn`] graceful-stop predicate.
    pub fn should_stop_after_turn(mut self, hook: Arc<dyn ShouldStopAfterTurn>) -> Self {
        self.should_stop_after_turn = Some(hook);
        self
    }

    /// Install an object-safe context-aware [`PrepareNextTurn`] hook.
    pub fn prepare_next_turn(mut self, hook: Arc<dyn PrepareNextTurn>) -> Self {
        self.prepare_next_turn = Some(hook);
        self
    }

    /// Install an object-safe [`QueueSource`] as the steering message source (polled instead of the
    /// built-in [`Agent::steer`] queue — see [`AgentConfig::with_steering_source_object`]).
    pub fn steering_source(mut self, source: Arc<dyn QueueSource>) -> Self {
        self.steering_source = Some(source);
        self
    }

    /// Install an object-safe [`QueueSource`] as the follow-up message source (polled instead of the
    /// built-in [`Agent::follow_up`] queue — see [`AgentConfig::with_follow_up_source_object`]).
    pub fn follow_up_source(mut self, source: Arc<dyn QueueSource>) -> Self {
        self.follow_up_source = Some(source);
        self
    }

    /// Assemble the configured [`Agent`].
    ///
    /// The setup's data becomes the agent's initial [`AgentState`] and the data-shaped fields of an
    /// [`AgentConfig`]; each attached `Arc<dyn Trait>` is adapted into the corresponding closure hook
    /// and installed on that config. `Option`/non-default config fields are applied only when set, so
    /// a default [`AgentSetup`] with no behavior yields exactly `AgentConfig::default()` apart from
    /// the data-derived `initial_state`.
    pub fn build(self) -> Agent {
        let AgentBuilder {
            setup,
            stream_fn,
            tools,
            convert_to_llm,
            transform_context,
            before_tool_call,
            after_tool_call,
            try_before_tool_call,
            try_after_tool_call,
            should_stop_after_turn,
            prepare_next_turn,
            steering_source,
            follow_up_source,
        } = self;

        // Data → initial state. `Agent::new` re-clears the transient streaming/pending/error fields,
        // so seeding them from `AgentState::default()` is safe and future-proof.
        let state = AgentState {
            system_prompt: setup.system_prompt,
            model: setup.model.into(),
            thinking_level: setup.thinking_level,
            tools,
            messages: setup.messages,
            ..AgentState::default()
        };

        // Data → config. Non-`Option` fields carry their (default-matching) values directly; each
        // `Option` field is applied only when set.
        let mut config = AgentConfig::default()
            .with_initial_state(state)
            .with_tool_execution(setup.tool_execution)
            .with_transport(setup.transport)
            .with_steering_mode(setup.steering_mode)
            .with_follow_up_mode(setup.follow_up_mode)
            .with_chat_options(setup.chat_options);

        if let Some(session_id) = setup.session_id {
            config = config.with_session_id(session_id);
        }
        if let Some(thinking_budgets) = setup.thinking_budgets {
            config = config.with_thinking_budgets(thinking_budgets);
        }
        if let Some(max_retries) = setup.max_retries {
            config = config.with_max_retries(max_retries);
        }
        if let Some(max_retry_delay_ms) = setup.max_retry_delay_ms {
            config = config.with_max_retry_delay_ms(max_retry_delay_ms);
        }

        // Behavior → trait-object registrations, via the same adapters `Agent::set_*_object` uses.
        if let Some(stream_fn) = stream_fn {
            config = config.with_stream_fn(stream_fn);
        }
        if let Some(converter) = convert_to_llm {
            config = config.with_convert_to_llm(message_converter_to_hook(converter));
        }
        if let Some(hook) = transform_context {
            config = config.with_transform_context(transform_context_to_hook(hook));
        }
        if let Some(hook) = before_tool_call {
            config = config.with_before_tool_call(before_tool_call_to_hook(hook));
        }
        if let Some(hook) = after_tool_call {
            config = config.with_after_tool_call(after_tool_call_to_hook(hook));
        }
        if let Some(hook) = try_before_tool_call {
            config = config.with_try_before_tool_call(try_before_tool_call_to_hook(hook));
        }
        if let Some(hook) = try_after_tool_call {
            config = config.with_try_after_tool_call(try_after_tool_call_to_hook(hook));
        }
        if let Some(hook) = should_stop_after_turn {
            config = config.with_should_stop_after_turn(should_stop_after_turn_to_hook(hook));
        }
        if let Some(hook) = prepare_next_turn {
            config = config.with_prepare_next_turn_with_context(prepare_next_turn_to_hook(hook));
        }
        if let Some(source) = steering_source {
            config = config.with_steering_source_object(source);
        }
        if let Some(source) = follow_up_source {
            config = config.with_follow_up_source_object(source);
        }

        Agent::new(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserMessage;

    #[test]
    fn agent_setup_round_trips_through_json() {
        let setup = AgentSetup {
            system_prompt: "you are helpful".to_string(),
            model: "mock-model".to_string(),
            session_id: Some("sess-1".to_string()),
            messages: vec![AgentMessage::User(UserMessage::text("hello"))],
            thinking_level: ThinkingLevel::Budget(4096),
            thinking_budgets: Some(ThinkingBudgets::default().with_high(400)),
            max_retries: Some(3),
            max_retry_delay_ms: Some(1_250),
            tool_execution: ToolExecutionMode::Sequential,
            transport: Transport::WebsocketCached,
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::OneAtATime,
            chat_options: ChatOptions::default(),
        };

        let json = serde_json::to_string(&setup).expect("AgentSetup serializes");
        let decoded: AgentSetup = serde_json::from_str(&json).expect("AgentSetup deserializes");

        // Field-level checks for readability...
        assert_eq!(decoded.system_prompt, setup.system_prompt);
        assert_eq!(decoded.model, setup.model);
        assert_eq!(decoded.session_id, setup.session_id);
        assert_eq!(decoded.messages, setup.messages);
        assert_eq!(decoded.thinking_level, setup.thinking_level);
        assert_eq!(decoded.thinking_budgets, setup.thinking_budgets);
        assert_eq!(decoded.max_retries, setup.max_retries);
        assert_eq!(decoded.max_retry_delay_ms, setup.max_retry_delay_ms);
        assert_eq!(decoded.tool_execution, setup.tool_execution);
        assert_eq!(decoded.transport, setup.transport);
        assert_eq!(decoded.steering_mode, setup.steering_mode);
        assert_eq!(decoded.follow_up_mode, setup.follow_up_mode);

        // ...and a whole-record re-serialization to also cover `chat_options` (no `PartialEq`).
        assert_eq!(
            serde_json::to_string(&decoded).expect("re-serializes"),
            json,
            "AgentSetup must round-trip byte-for-byte"
        );
    }

    #[test]
    fn default_setup_yields_default_config_shape() {
        // A default `AgentSetup` with no behavior must produce a config equal to
        // `AgentConfig::default()` on every data-shaped field (aside from `initial_state`).
        let built = AgentBuilder::new(AgentSetup::default()).build();
        let state = built.state();
        assert_eq!(state.system_prompt, "");
        assert!(state.messages.is_empty());
        assert!(state.tools.is_empty());
        assert_eq!(state.thinking_level, ThinkingLevel::Off);
        // The empty model slug becomes a `ModelSpec::Name("")`.
        assert!(format!("{:?}", state.model).contains("Name"));
        assert_eq!(built.steering_mode(), QueueMode::OneAtATime);
        assert_eq!(built.follow_up_mode(), QueueMode::OneAtATime);
    }

    // End-to-end: build entirely from data (`AgentSetup`) + behavior as trait objects
    // (`MockStreamFn` stream function, a `FnTool`, and a `BeforeToolCall` object hook), run a
    // prompt, and prove the whole construct-from-data / attach-behavior path.
    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn agent_builder_assembles_data_and_trait_object_behavior() {
        use crate::testing::{MockStreamFn, ScriptedStream, fixtures, script, tools};
        use crate::{
            BeforeToolCallContext, BeforeToolCallOutcome, CancellationToken, StopReason,
            ToolResultContent,
        };
        use async_trait::async_trait;
        use serde_json::json;
        use std::sync::atomic::{AtomicBool, Ordering};

        // A `BeforeToolCall` object hook that rewrites the calculator's expression in place. If it
        // takes effect, the tool result reflects the rewritten expression rather than the original.
        struct RewriteExpression {
            called: Arc<AtomicBool>,
        }

        #[async_trait]
        impl BeforeToolCall for RewriteExpression {
            async fn before(
                &self,
                ctx: BeforeToolCallContext,
                _cancel: CancellationToken,
            ) -> BeforeToolCallOutcome {
                self.called.store(true, Ordering::SeqCst);
                let mut args = ctx.args.clone();
                if let Some(object) = args.as_object_mut() {
                    object.insert("expression".to_string(), json!("2 + 2"));
                }
                BeforeToolCallOutcome {
                    args: Some(args),
                    decision: None,
                }
            }
        }

        let seed_messages = vec![
            fixtures::user_msg("prior question"),
            AgentMessage::Assistant(fixtures::text_msg("prior answer")),
        ];

        let setup = AgentSetup {
            system_prompt: "You are a helpful assistant.".to_string(),
            model: "mock-model".to_string(),
            messages: seed_messages.clone(),
            thinking_level: ThinkingLevel::Low,
            ..AgentSetup::default()
        };

        let tool_turn = ScriptedStream::from_message(fixtures::assistant_msg(
            vec![
                script::text("Let me calculate."),
                script::tool_call("calc-1", "calculate", json!({ "expression": "1 + 1" })),
            ],
            StopReason::ToolUse,
        ));
        let final_turn = ScriptedStream::from_message(fixtures::assistant_msg(
            vec![script::text("All done.")],
            StopReason::Stop,
        ));
        let stream_fn = Arc::new(MockStreamFn::from_streams(vec![tool_turn, final_turn]));

        let called = Arc::new(AtomicBool::new(false));
        let agent = AgentBuilder::new(setup)
            .stream_fn(stream_fn)
            .tool(tools::calculate_tool())
            .before_tool_call(Arc::new(RewriteExpression {
                called: called.clone(),
            }))
            .build();

        // The setup's data landed in the agent's state before any run.
        let built = agent.state();
        assert_eq!(built.system_prompt, "You are a helpful assistant.");
        assert_eq!(built.thinking_level, ThinkingLevel::Low);
        assert!(
            format!("{:?}", built.model).contains("mock-model"),
            "model should carry the setup slug: {:?}",
            built.model
        );
        assert_eq!(built.messages, seed_messages);
        assert_eq!(built.tools.len(), 1);
        assert_eq!(built.tools[0].spec().name, "calculate");

        // Behavior attached as trait objects drives a real run.
        agent.prompt("Please calculate.").await.unwrap();

        assert!(
            called.load(Ordering::SeqCst),
            "the before-tool-call object hook ran"
        );

        let state = agent.state();
        let tool_result = state
            .messages
            .iter()
            .find_map(|message| match message {
                AgentMessage::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("a tool-result message");
        let text: String = tool_result
            .content
            .iter()
            .filter_map(|block| match block {
                ToolResultContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("2 + 2 = 4"),
            "the tool ran with the hook-rewritten args: {text}"
        );
        assert!(
            !text.contains("1 + 1"),
            "the hook rewrote the args before execution: {text}"
        );

        // Seed exchange (2) + new user prompt + assistant tool call + tool result + final answer.
        assert!(state.messages.len() >= 6, "{:?}", state.messages);
        assert!(!state.is_streaming);
    }
}
