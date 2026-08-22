//! Executable tool contracts needed by event records and snapshot restoration
//! (Architecture v2 part 1 §4.5 and part 2 §9.2).

use crate::AgentError;
use pi_ai::{
    CancellationToken, LocalBoxFuture, MessageId, SendBoxFuture, ToolCall, ToolResultContent,
    ToolSpec, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{collections::BTreeMap, fmt, rc::Rc, sync::Arc};

/// Scheduling requirement for an executable tool.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    /// The call may execute concurrently with other asynchronous calls.
    #[default]
    Parallel,
    /// Any occurrence forces the complete assistant tool batch to run in
    /// source order.
    Sequential,
}

/// Stable semantic inputs supplied to one tool execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallContext {
    /// Assistant message that requested the call.
    pub assistant_message_id: MessageId,
    /// Finalized provider-neutral call and validated argument value.
    pub call: ToolCall,
}

/// Final executable tool result before it becomes a canonical tool-result
/// transcript message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Model-visible text and image content.
    pub content: Vec<ToolResultContent>,
    /// Version-neutral tool-owned JSON details.
    pub details: Option<Box<RawValue>>,
    /// Usage attributable to the tool itself.
    pub usage: Option<Usage>,
    /// Tool names made available after committing this result.
    pub added_tool_names: Vec<String>,
    /// Hint to stop automatic continuation after the complete batch.
    pub terminate: bool,
}

impl PartialEq for ToolOutput {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content
            && raw_values_equal(self.details.as_deref(), other.details.as_deref())
            && self.usage == other.usage
            && self.added_tool_names == other.added_tool_names
            && self.terminate == other.terminate
    }
}

impl ToolOutput {
    /// Creates ordinary non-terminating output with no details or usage.
    pub fn new(content: Vec<ToolResultContent>) -> Self {
        Self {
            content,
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        }
    }
}

/// One transient partial tool result emitted before finalization.
///
/// It intentionally mirrors the final output shape because Pi allows tools to
/// stream the same result contract. The value is event data, not durable agent
/// state until a final tool-result message is committed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolUpdate {
    /// Partial model- or UI-visible content.
    pub content: Vec<ToolResultContent>,
    /// Partial tool-owned JSON details.
    pub details: Option<Box<RawValue>>,
    /// Last tool-reported usage observation.
    pub usage: Option<Usage>,
    /// Tool names reported by this observation.
    pub added_tool_names: Vec<String>,
    /// Partial termination hint.
    pub terminate: bool,
}

impl PartialEq for ToolUpdate {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content
            && raw_values_equal(self.details.as_deref(), other.details.as_deref())
            && self.usage == other.usage
            && self.added_tool_names == other.added_tool_names
            && self.terminate == other.terminate
    }
}

fn raw_values_equal(left: Option<&RawValue>, right: Option<&RawValue>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.get() == right.get(),
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

impl From<ToolOutput> for ToolUpdate {
    fn from(output: ToolOutput) -> Self {
        Self {
            content: output.content,
            details: output.details,
            usage: output.usage,
            added_tool_names: output.added_tool_names,
            terminate: output.terminate,
        }
    }
}

/// Sanitized failure returned by an executable tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolError {
    /// Stable application-defined error code.
    pub code: String,
    /// Human-readable diagnostic safe to show to the model.
    pub message: String,
}

impl ToolError {
    /// Creates a tool failure.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

/// Failure to accept a transient tool update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolUpdateError {
    /// Sanitized sink diagnostic.
    pub message: String,
}

impl ToolUpdateError {
    /// Creates an update-sink failure.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ToolUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolUpdateError {}

/// Thread-safe receiver for transient tool execution updates.
pub trait ToolUpdateSink: Send + Sync + 'static {
    /// Accepts one update while the tool invocation remains active.
    fn update(&self, update: ToolUpdate) -> Result<(), ToolUpdateError>;
}

/// Single-threaded receiver for transient tool execution updates.
pub trait LocalToolUpdateSink: 'static {
    /// Accepts one update while the tool invocation remains active.
    fn update(&self, update: ToolUpdate) -> Result<(), ToolUpdateError>;
}

/// Thread-safe executable tool family for native and multithreaded runtimes.
pub trait Tool: Send + Sync + 'static {
    /// Returns the provider-neutral model-facing specification.
    fn spec(&self) -> &ToolSpec;

    /// Returns this tool's scheduling requirement.
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    /// Executes one finalized and validated call.
    fn execute(
        &self,
        context: ToolCallContext,
        updates: Arc<dyn ToolUpdateSink>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, ToolError>>;
}

/// Single-threaded executable tool family for local and WASM runtimes.
pub trait LocalTool: 'static {
    /// Returns the provider-neutral model-facing specification.
    fn spec(&self) -> &ToolSpec;

    /// Returns this tool's scheduling requirement.
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    /// Executes one finalized and validated call without requiring `Send`.
    fn execute(
        &self,
        context: ToolCallContext,
        updates: Rc<dyn LocalToolUpdateSink>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ToolOutput, ToolError>>;
}

/// Bound thread-safe executable tools indexed by model-facing name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds one executable tool, rejecting empty or duplicate names.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), AgentError> {
        let name = tool.spec().name.clone();
        if name.is_empty() {
            return Err(AgentError::InvalidToolName);
        }
        if self.tools.contains_key(&name) {
            return Err(AgentError::DuplicateToolName { name });
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Resolves an executable tool by model-facing name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Returns whether no executable tools are bound.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Returns the number of bound tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Tool>)> {
        self.tools.iter().map(|(name, tool)| (name.as_str(), tool))
    }

    pub(crate) fn validate(&self) -> Result<(), AgentError> {
        if self.tools.keys().any(String::is_empty) {
            return Err(AgentError::InvalidToolName);
        }
        Ok(())
    }
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field("names", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Bound local executable tools indexed by model-facing name.
#[derive(Clone, Default)]
pub struct LocalToolRegistry {
    tools: BTreeMap<String, Rc<dyn LocalTool>>,
}

impl LocalToolRegistry {
    /// Creates an empty local registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds one local executable tool, rejecting empty or duplicate names.
    pub fn register(&mut self, tool: Rc<dyn LocalTool>) -> Result<(), AgentError> {
        let name = tool.spec().name.clone();
        if name.is_empty() {
            return Err(AgentError::InvalidToolName);
        }
        if self.tools.contains_key(&name) {
            return Err(AgentError::DuplicateToolName { name });
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Resolves a local executable tool by model-facing name.
    pub fn get(&self, name: &str) -> Option<&Rc<dyn LocalTool>> {
        self.tools.get(name)
    }

    /// Returns whether no local executable tools are bound.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Returns the number of bound local tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &Rc<dyn LocalTool>)> {
        self.tools.iter().map(|(name, tool)| (name.as_str(), tool))
    }

    pub(crate) fn validate(&self) -> Result<(), AgentError> {
        if self.tools.keys().any(String::is_empty) {
            return Err(AgentError::InvalidToolName);
        }
        Ok(())
    }
}

impl fmt::Debug for LocalToolRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalToolRegistry")
            .field("names", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}
