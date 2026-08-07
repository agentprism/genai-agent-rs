//! Tool metadata, execution interfaces, closure adapters, and partial-update delivery.
//!
//! Tools expose provider-facing JSON Schema through [`ToolSpec`], receive validated arguments in
//! [`ToolCallContext`], and return transcript-ready [`AgentToolResult`] values. [`UpdateSink`]
//! provides ordered partial updates with an explicit settlement boundary.

use crate::{AgentToolCall, AgentUsage, ToolError, ToolExecutionMode, ToolResultContent};
use async_trait::async_trait;
use futures::future::BoxFuture;
use genai::chat::Tool;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex, PoisonError};
use tokio_util::sync::CancellationToken;

/// Provider-visible metadata and UI label for an agent tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Stable provider-facing function name used to match assistant calls.
    pub name: String,
    /// Human-readable label for application interfaces; not sent by [`Self::to_genai`].
    pub label: String,
    /// Provider-facing description of the tool's behavior.
    pub description: String,
    /// JSON Schema used both for provider declaration and local argument validation/coercion.
    pub schema: Value,
    /// Optional provider strict-schema flag.
    pub strict: Option<bool>,
}

impl ToolSpec {
    /// Construct a specification whose display label initially matches its name.
    pub fn new(name: impl Into<String>, description: impl Into<String>, schema: Value) -> Self {
        let name = name.into();
        Self {
            label: name.clone(),
            name,
            description: description.into(),
            schema,
            strict: None,
        }
    }

    /// Set the application-facing display label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Set the optional provider strict-schema flag.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }

    /// Convert this specification into the provider-native `genai` tool declaration.
    ///
    /// The application-only [`Self::label`] is intentionally omitted.
    pub fn to_genai(&self) -> Tool {
        let mut tool = Tool::new(self.name.clone())
            .with_description(self.description.clone())
            .with_schema(self.schema.clone());
        if let Some(strict) = self.strict {
            tool = tool.with_strict(strict);
        }
        tool
    }
}

/// Owned execution context for one assistant tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallContext {
    /// Provider-assigned identifier for correlating the eventual result.
    pub tool_call_id: String,
    /// Name of the tool selected by the assistant.
    pub tool_name: String,
    /// Prepared, validated, and coerced JSON arguments supplied for execution.
    pub args: Value,
}

impl ToolCallContext {
    /// Construct an owned execution context.
    pub fn new(tool_call_id: impl Into<String>, tool_name: impl Into<String>, args: Value) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            args,
        }
    }
}

impl From<AgentToolCall> for ToolCallContext {
    fn from(value: AgentToolCall) -> Self {
        Self::new(value.id, value.name, value.arguments)
    }
}

/// Final or partial output from an agent tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentToolResult {
    /// Ordered text and image blocks returned by the tool.
    pub content: Vec<ToolResultContent>,
    /// Application-defined structured detail retained in events and the transcript.
    pub details: Value,
    /// Optional resource usage attributable to this execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsage>,
    /// Names of tools made available as a consequence of this result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_tool_names: Vec<String>,
    /// Whether this result requests agent-loop termination under the active batch policy.
    #[serde(default)]
    pub terminate: bool,
}

impl AgentToolResult {
    /// Construct a non-terminating result without usage or added-tool metadata.
    pub fn new(content: Vec<ToolResultContent>, details: Value) -> Self {
        Self {
            content,
            details,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        }
    }

    /// Construct a result containing one text block and null structured details.
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(vec![ToolResultContent::text(text)], Value::Null)
    }

    /// Construct a result whose text is compact JSON and whose details retain the JSON value.
    pub fn json(value: Value) -> Self {
        Self::new(vec![ToolResultContent::text(value.to_string())], value)
    }

    /// Attach resource usage to the result.
    pub fn with_usage(mut self, usage: AgentUsage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Set whether this result requests agent-loop termination.
    pub fn with_terminate(mut self, terminate: bool) -> Self {
        self.terminate = terminate;
        self
    }
}

impl Default for AgentToolResult {
    fn default() -> Self {
        Self::new(Vec::new(), Value::Null)
    }
}

/// Cloneable, settlement-scoped callback for partial tool-execution updates.
///
/// Emission and closure are serialized across every clone by one non-reentrant gate. An `emit`
/// racing with [`Self::close`] is linearized entirely before or after it: if accepted, the callback
/// finishes before `close` returns; otherwise the update is ignored. Dropping an individual clone
/// does not close the sink.
///
/// During agent execution, the callback registers accepted updates on an unbounded producer queue.
/// This keeps [`Self::emit`] synchronous and free of async backpressure, but a tool that can outpace
/// event listeners should bound or coalesce its own updates. The runtime settles every registered
/// update through the awaited event sink before publishing that tool's final result.
///
/// The callback passed to [`Self::new`] runs while the shared gate is held. It **must not** call
/// `emit`, `close`, or `is_closed` on the same sink or any clone; such same-sink re-entry attempts to
/// acquire the non-reentrant gate again and deadlocks.
#[derive(Clone)]
pub struct UpdateSink {
    state: Arc<Mutex<UpdateSinkState>>,
}

struct UpdateSinkState {
    callback: Arc<dyn Fn(AgentToolResult) + Send + Sync>,
    closed: bool,
}

impl std::fmt::Debug for UpdateSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateSink")
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl UpdateSink {
    /// Create an open sink that invokes `callback` synchronously for each accepted update.
    ///
    /// Callbacks across all clones are serialized and are subject to the same-sink non-reentry rule
    /// described on [`UpdateSink`].
    pub fn new(callback: impl Fn(AgentToolResult) + Send + Sync + 'static) -> Self {
        Self {
            state: Arc::new(Mutex::new(UpdateSinkState {
                callback: Arc::new(callback),
                closed: false,
            })),
        }
    }

    /// Create an open sink that accepts and discards updates until closed.
    pub fn noop() -> Self {
        Self::new(|_| {})
    }

    /// Deliver an update synchronously if settlement has not closed the sink.
    ///
    /// Returns `true` only after the callback has finished accepting the update. Returns `false`
    /// without invoking the callback when the sink was already closed. This method does not wait
    /// for the agent's asynchronous event listener; the agent runtime performs that settlement
    /// before its final tool-result event.
    pub fn emit(&self, update: AgentToolResult) -> bool {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return false;
        }
        (state.callback)(update);
        true
    }

    /// Idempotently close this sink and all of its clones.
    ///
    /// When this returns, every emission linearized before the close has finished its callback and
    /// all later emissions will return `false`.
    pub fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
    }

    /// Return whether this sink has been closed through any clone.
    pub fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed
    }
}

/// Object-safe executable agent tool.
///
/// Tool implementations may be called concurrently when execution policy permits, so both the
/// object and returned futures must be safe to send between tasks.
#[async_trait]
pub trait AgentTool: Send + Sync {
    /// Return the provider-facing specification and local validation schema.
    fn spec(&self) -> ToolSpec;

    /// Optionally override the loop's tool-execution mode for this tool.
    ///
    /// `None` inherits the loop-wide mode.
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    /// Transform raw assistant arguments immediately before the single validation/coercion pass.
    ///
    /// The default is identity. This compatibility hook is suitable for repairing provider-specific
    /// argument shapes; it should not assume that `args` already satisfies [`Self::spec`].
    fn prepare_arguments(&self, args: Value) -> Value {
        args
    }

    /// Execute one prepared call.
    ///
    /// Implementations should observe `cancel` cooperatively and may report partial results through
    /// `on_update`. The runtime closes the update sink at settlement and converts a returned
    /// [`ToolError`] into an error tool-result message rather than failing the entire agent loop.
    async fn execute(
        &self,
        call: ToolCallContext,
        cancel: CancellationToken,
        on_update: UpdateSink,
    ) -> Result<AgentToolResult, ToolError>;
}

type ExecuteFn = Arc<
    dyn Fn(
            ToolCallContext,
            CancellationToken,
            UpdateSink,
        ) -> BoxFuture<'static, Result<AgentToolResult, ToolError>>
        + Send
        + Sync,
>;
type PrepareArgumentsFn = Arc<dyn Fn(Value) -> Value + Send + Sync>;

/// Closure-backed [`AgentTool`] implementation for applications and tests.
pub struct FnTool {
    spec: ToolSpec,
    execution_mode: Option<ToolExecutionMode>,
    prepare_arguments: Option<PrepareArgumentsFn>,
    execute: ExecuteFn,
}

impl std::fmt::Debug for FnTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FnTool")
            .field("spec", &self.spec)
            .field("execution_mode", &self.execution_mode)
            .finish_non_exhaustive()
    }
}

impl FnTool {
    /// Build a tool from an async closure receiving the full execution context.
    pub fn new<F, Fut>(spec: ToolSpec, execute: F) -> Self
    where
        F: Fn(ToolCallContext, CancellationToken, UpdateSink) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<AgentToolResult, ToolError>> + Send + 'static,
    {
        Self {
            spec,
            execution_mode: None,
            prepare_arguments: None,
            execute: Arc::new(move |call, cancel, updates| {
                Box::pin(execute(call, cancel, updates))
            }),
        }
    }

    /// Build a tool from an arguments-only async closure.
    pub fn from_value_fn<F, Fut>(spec: ToolSpec, execute: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<AgentToolResult, ToolError>> + Send + 'static,
    {
        Self::new(spec, move |call, _cancel, _updates| execute(call.args))
    }

    /// Override the loop-wide execution mode for this tool.
    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = Some(mode);
        self
    }

    /// Install a raw-argument transform that runs before validation and coercion.
    pub fn with_prepare_arguments(
        mut self,
        prepare: impl Fn(Value) -> Value + Send + Sync + 'static,
    ) -> Self {
        self.prepare_arguments = Some(Arc::new(prepare));
        self
    }
}

#[async_trait]
impl AgentTool for FnTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }

    fn prepare_arguments(&self, args: Value) -> Value {
        self.prepare_arguments
            .as_ref()
            .map_or(args.clone(), |prepare| prepare(args))
    }

    async fn execute(
        &self,
        call: ToolCallContext,
        cancel: CancellationToken,
        on_update: UpdateSink,
    ) -> Result<AgentToolResult, ToolError> {
        let result = (self.execute)(call, cancel, on_update.clone()).await;
        on_update.close();
        result
    }
}

/// Compact macro form of [`FnTool::new`].
#[macro_export]
macro_rules! tool_fn {
    ($spec:expr, $execute:expr) => {
        $crate::FnTool::new($spec, $execute)
    };
}
