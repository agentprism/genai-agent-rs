//! Executable tool contracts, typed adapters, schema validation, and registries
//! (Architecture v2 part 1 §4.5–§4.6 and part 2 §8.1, §9.2).

use crate::AgentError;
use agentprism_ai::{
    CancellationToken, LocalBoxFuture, MessageId, SendBoxFuture, ToolCall, ToolResultContent,
    ToolSpec, Usage,
};
use indexmap::IndexMap;
use jsonschema::Validator;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, value::RawValue};
use std::{fmt, marker::PhantomData, rc::Rc, sync::Arc};

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
    /// Finalized provider-neutral call with normalized and validated arguments.
    pub call: ToolCall,
    /// Effective arguments at the validation boundary, before an authorization
    /// policy may perform Pi-compatible post-validation mutation.
    /// This is transient validation evidence and is never serialized.
    #[serde(skip)]
    pub validated_arguments: Value,
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
    ///
    /// Core stream adapters use bounded buffering. A synchronously chatty tool
    /// receives [`ToolUpdateError`] when that buffer is saturated and may retry
    /// after yielding. Updates submitted after execution settles are ignored.
    fn update(&self, update: ToolUpdate) -> Result<(), ToolUpdateError>;
}

/// Single-threaded receiver for transient tool execution updates.
pub trait LocalToolUpdateSink: 'static {
    /// Accepts one update with the same bounded-buffer contract as
    /// [`ToolUpdateSink`].
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

/// Compatibility argument preparation performed before JSON Schema validation.
///
/// This is a registry binding concern rather than a method on [`Tool`], keeping
/// the executable trait at the exact narrow shape in architecture §4.5.
pub trait ToolArgumentPreparer: Send + Sync + 'static {
    /// Returns effective arguments for schema validation and execution.
    fn prepare(&self, arguments: &Value) -> Result<Value, ToolError>;
}

impl<F> ToolArgumentPreparer for F
where
    F: Fn(&Value) -> Result<Value, ToolError> + Send + Sync + 'static,
{
    fn prepare(&self, arguments: &Value) -> Result<Value, ToolError> {
        self(arguments)
    }
}

/// Local-executor compatibility argument preparation.
pub trait LocalToolArgumentPreparer: 'static {
    /// Returns effective arguments for schema validation and execution.
    fn prepare(&self, arguments: &Value) -> Result<Value, ToolError>;
}

impl<F> LocalToolArgumentPreparer for F
where
    F: Fn(&Value) -> Result<Value, ToolError> + 'static,
{
    fn prepare(&self, arguments: &Value) -> Result<Value, ToolError> {
        self(arguments)
    }
}

/// Typed asynchronous tool adapter using its generated schema as [`ToolSpec`].
///
/// `F` receives the call context with the same normalized JSON that produced
/// `I`, the typed input, the update sink, and the portable cancellation token.
/// It returns an owned boxed future so ordinary async closures can be stored in
/// the heterogeneous registry without another public future generic.
pub struct TypedTool<I, F> {
    spec: ToolSpec,
    validator: Arc<Validator>,
    function: F,
    execution_mode: ToolExecutionMode,
    input: PhantomData<fn() -> I>,
}

impl<I, F> TypedTool<I, F>
where
    I: DeserializeOwned + JsonSchema + Send + 'static,
{
    /// Generates `parameters` from `I` and constructs a typed tool.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        function: F,
    ) -> Result<Self, AgentError> {
        let spec = ToolSpec {
            schema_version: 1,
            name: name.into(),
            description: description.into(),
            parameters: serde_json::to_value(schemars::schema_for!(I)).map_err(|error| {
                AgentError::InvalidConfiguration {
                    message: format!("could not serialize generated tool schema: {error}"),
                }
            })?,
            constrained_sampling: None,
        };
        Self::from_spec(spec, function)
    }

    /// Uses an explicit model-facing specification while retaining typed input.
    /// Arguments are validated against this specification before deserializing
    /// as `I`, allowing stricter constraints than derive-generated structure.
    pub fn from_spec(spec: ToolSpec, function: F) -> Result<Self, AgentError> {
        validate_tool_name(&spec.name)?;
        let validator = compile_schema(&spec)?;
        Ok(Self {
            spec,
            validator,
            function,
            execution_mode: ToolExecutionMode::Parallel,
            input: PhantomData,
        })
    }

    /// Selects this tool's scheduling requirement.
    #[must_use]
    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = mode;
        self
    }
}

impl<I, F> Tool for TypedTool<I, F>
where
    I: DeserializeOwned + JsonSchema + Send + 'static,
    F: Fn(
            ToolCallContext,
            I,
            Arc<dyn ToolUpdateSink>,
            CancellationToken,
        ) -> SendBoxFuture<'static, Result<ToolOutput, ToolError>>
        + Send
        + Sync
        + 'static,
{
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.execution_mode
    }

    fn execute(
        &self,
        context: ToolCallContext,
        updates: Arc<dyn ToolUpdateSink>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ToolOutput, ToolError>> {
        if let Err(error) =
            validate_arguments(&self.spec, &self.validator, &context.validated_arguments)
        {
            return Box::pin(async move { Err(error) });
        }
        let input = match serde_json::from_value::<I>(context.call.arguments.clone()) {
            Ok(input) => input,
            Err(error) => {
                let tool_error = ToolError::new(
                    "tool_deserialization",
                    format!(
                        "Validated arguments for tool \"{}\" could not be deserialized: {error}",
                        self.spec.name
                    ),
                );
                return Box::pin(async move { Err(tool_error) });
            }
        };
        (self.function)(context, input, updates, cancellation)
    }
}

/// Local/WASM typed asynchronous tool adapter.
pub struct LocalTypedTool<I, F> {
    spec: ToolSpec,
    validator: Rc<Validator>,
    function: F,
    execution_mode: ToolExecutionMode,
    input: PhantomData<fn() -> I>,
}

impl<I, F> LocalTypedTool<I, F>
where
    I: DeserializeOwned + JsonSchema + 'static,
{
    /// Generates `parameters` from `I` and constructs a local typed tool.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        function: F,
    ) -> Result<Self, AgentError> {
        let spec = ToolSpec {
            schema_version: 1,
            name: name.into(),
            description: description.into(),
            parameters: serde_json::to_value(schemars::schema_for!(I)).map_err(|error| {
                AgentError::InvalidConfiguration {
                    message: format!("could not serialize generated local tool schema: {error}"),
                }
            })?,
            constrained_sampling: None,
        };
        Self::from_spec(spec, function)
    }

    /// Uses an explicit specification with local typed execution.
    pub fn from_spec(spec: ToolSpec, function: F) -> Result<Self, AgentError> {
        validate_tool_name(&spec.name)?;
        let validator = Rc::new(compile_schema_value(&spec)?);
        Ok(Self {
            spec,
            validator,
            function,
            execution_mode: ToolExecutionMode::Parallel,
            input: PhantomData,
        })
    }

    /// Selects this local tool's scheduling requirement.
    #[must_use]
    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = mode;
        self
    }
}

impl<I, F> LocalTool for LocalTypedTool<I, F>
where
    I: DeserializeOwned + JsonSchema + 'static,
    F: Fn(
            ToolCallContext,
            I,
            Rc<dyn LocalToolUpdateSink>,
            CancellationToken,
        ) -> LocalBoxFuture<'static, Result<ToolOutput, ToolError>>
        + 'static,
{
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.execution_mode
    }

    fn execute(
        &self,
        context: ToolCallContext,
        updates: Rc<dyn LocalToolUpdateSink>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ToolOutput, ToolError>> {
        if let Err(error) =
            validate_arguments(&self.spec, &self.validator, &context.validated_arguments)
        {
            return Box::pin(async move { Err(error) });
        }
        let input = match serde_json::from_value::<I>(context.call.arguments.clone()) {
            Ok(input) => input,
            Err(error) => {
                let tool_error = ToolError::new(
                    "tool_deserialization",
                    format!(
                        "Validated arguments for local tool \"{}\" could not be deserialized: {error}",
                        self.spec.name
                    ),
                );
                return Box::pin(async move { Err(tool_error) });
            }
        };
        (self.function)(context, input, updates, cancellation)
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredTool {
    pub(crate) tool: Arc<dyn Tool>,
    pub(crate) validator: Arc<Validator>,
    pub(crate) preparer: Option<Arc<dyn ToolArgumentPreparer>>,
}

/// Bound thread-safe executable tools indexed by model-facing name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: IndexMap<String, RegisteredTool>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds one executable tool using identity argument preparation.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), AgentError> {
        self.register_inner(tool, None)
    }

    /// Binds one executable tool with a compatibility argument preparer.
    pub fn register_with_argument_preparer(
        &mut self,
        tool: Arc<dyn Tool>,
        preparer: Arc<dyn ToolArgumentPreparer>,
    ) -> Result<(), AgentError> {
        self.register_inner(tool, Some(preparer))
    }

    fn register_inner(
        &mut self,
        tool: Arc<dyn Tool>,
        preparer: Option<Arc<dyn ToolArgumentPreparer>>,
    ) -> Result<(), AgentError> {
        let name = tool.spec().name.clone();
        validate_tool_name(&name)?;
        if self.tools.contains_key(&name) {
            return Err(AgentError::DuplicateToolName { name });
        }
        let validator = compile_schema(tool.spec())?;
        self.tools.insert(
            name,
            RegisteredTool {
                tool,
                validator,
                preparer,
            },
        );
        Ok(())
    }

    /// Resolves an executable tool by model-facing name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name).map(|registered| &registered.tool)
    }

    /// Returns whether no executable tools are bound.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Returns the number of bound tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub(crate) fn binding(&self, name: &str) -> Option<&RegisteredTool> {
        self.tools.get(name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Tool>)> {
        self.tools
            .iter()
            .map(|(name, registered)| (name.as_str(), &registered.tool))
    }

    pub(crate) fn validate(&self) -> Result<(), AgentError> {
        for (name, registered) in &self.tools {
            validate_tool_name(name)?;
            if registered.tool.spec().name != *name {
                return Err(AgentError::InvariantViolation {
                    message: format!(
                        "tool registry key {name} no longer matches its specification"
                    ),
                });
            }
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

#[derive(Clone)]
pub(crate) struct RegisteredLocalTool {
    pub(crate) tool: Rc<dyn LocalTool>,
    pub(crate) validator: Rc<Validator>,
    pub(crate) preparer: Option<Rc<dyn LocalToolArgumentPreparer>>,
}

/// Bound local executable tools indexed by model-facing name.
#[derive(Clone, Default)]
pub struct LocalToolRegistry {
    tools: IndexMap<String, RegisteredLocalTool>,
}

impl LocalToolRegistry {
    /// Creates an empty local registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds one local executable tool using identity argument preparation.
    pub fn register(&mut self, tool: Rc<dyn LocalTool>) -> Result<(), AgentError> {
        self.register_inner(tool, None)
    }

    /// Binds one local tool with a compatibility argument preparer.
    pub fn register_with_argument_preparer(
        &mut self,
        tool: Rc<dyn LocalTool>,
        preparer: Rc<dyn LocalToolArgumentPreparer>,
    ) -> Result<(), AgentError> {
        self.register_inner(tool, Some(preparer))
    }

    fn register_inner(
        &mut self,
        tool: Rc<dyn LocalTool>,
        preparer: Option<Rc<dyn LocalToolArgumentPreparer>>,
    ) -> Result<(), AgentError> {
        let name = tool.spec().name.clone();
        validate_tool_name(&name)?;
        if self.tools.contains_key(&name) {
            return Err(AgentError::DuplicateToolName { name });
        }
        let validator = Rc::new(compile_schema_value(tool.spec())?);
        self.tools.insert(
            name,
            RegisteredLocalTool {
                tool,
                validator,
                preparer,
            },
        );
        Ok(())
    }

    /// Resolves a local executable tool by model-facing name.
    pub fn get(&self, name: &str) -> Option<&Rc<dyn LocalTool>> {
        self.tools.get(name).map(|registered| &registered.tool)
    }

    /// Returns whether no local executable tools are bound.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Returns the number of bound local tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub(crate) fn binding(&self, name: &str) -> Option<&RegisteredLocalTool> {
        self.tools.get(name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &Rc<dyn LocalTool>)> {
        self.tools
            .iter()
            .map(|(name, registered)| (name.as_str(), &registered.tool))
    }

    pub(crate) fn validate(&self) -> Result<(), AgentError> {
        for (name, registered) in &self.tools {
            validate_tool_name(name)?;
            if registered.tool.spec().name != *name {
                return Err(AgentError::InvariantViolation {
                    message: format!(
                        "local tool registry key {name} no longer matches its specification"
                    ),
                });
            }
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

fn validate_tool_name(name: &str) -> Result<(), AgentError> {
    if name.is_empty() {
        Err(AgentError::InvalidToolName)
    } else {
        Ok(())
    }
}

fn compile_schema(spec: &ToolSpec) -> Result<Arc<Validator>, AgentError> {
    compile_schema_value(spec).map(Arc::new)
}

fn compile_schema_value(spec: &ToolSpec) -> Result<Validator, AgentError> {
    jsonschema::validator_for(&spec.parameters).map_err(|error| AgentError::InvalidToolSchema {
        name: spec.name.clone(),
        message: error.to_string(),
    })
}

pub(crate) fn validate_arguments(
    spec: &ToolSpec,
    validator: &Validator,
    arguments: &Value,
) -> Result<Value, ToolError> {
    // Pi validates a structured clone, not the canonical tool call value. Its
    // validation boundary first removes optional non-nullable nulls and then
    // applies TypeBox/AJV-compatible coercion. Returning that clone is
    // important: authorization and execution must observe the normalized
    // value while the committed assistant call remains unchanged.
    let mut normalized = arguments.clone();
    normalize_optional_nulls(&mut normalized, &spec.parameters);
    normalized = coerce_with_json_schema(normalized, &spec.parameters);

    let errors = validator
        .iter_errors(&normalized)
        .map(|error| {
            let path = error.instance_path().to_string();
            let path = if path.is_empty() {
                "root"
            } else {
                path.as_str()
            };
            format!("  - {path}: {error}")
        })
        .collect::<Vec<_>>();
    if errors.is_empty() {
        return Ok(normalized);
    }
    Err(ToolError::new(
        "tool_validation",
        format!(
            "Validation failed for tool \"{}\":\n{}\n\nReceived arguments:\n{}",
            spec.name,
            errors.join("\n"),
            serde_json::to_string_pretty(arguments)
                .unwrap_or_else(|_| "<unserializable JSON value>".into())
        ),
    ))
}

fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    if let Value::Array(values) = value {
        match schema.get("items") {
            Some(Value::Array(item_schemas)) => {
                for (item, item_schema) in values.iter_mut().zip(item_schemas) {
                    normalize_optional_nulls(item, item_schema);
                }
            }
            Some(item_schema @ Value::Object(_)) => {
                for item in values {
                    normalize_optional_nulls(item, item_schema);
                }
            }
            Some(_) | None => {}
        }
        return;
    }

    let (Value::Object(object), Some(Value::Object(properties))) =
        (value, schema.get("properties"))
    else {
        return;
    };
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<std::collections::BTreeSet<_>>();

    for (key, property_schema) in properties {
        let should_remove = object.get(key).is_some_and(Value::is_null)
            && !required.contains(key.as_str())
            // Pi deliberately preserves a direct $ref here. Compiling the
            // isolated referenced property would lose the root definition
            // context, and nullable references must survive normalization.
            && property_schema.get("$ref").and_then(Value::as_str).is_none()
            && sub_schema_accepts(property_schema, &Value::Null) == Some(false);
        if should_remove {
            object.remove(key);
        } else if let Some(property_value) = object.get_mut(key) {
            normalize_optional_nulls(property_value, property_schema);
        }
    }
}

fn coerce_with_json_schema(mut value: Value, schema: &Value) -> Value {
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for nested in all_of {
            value = coerce_with_json_schema(value, nested);
        }
    }
    if let Some(any_of) = schema.get("anyOf").and_then(Value::as_array) {
        value = coerce_with_union_schema(value, any_of);
    }
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        value = coerce_with_union_schema(value, one_of);
    }

    let schema_types = schema_types(schema);
    let already_matches_union = schema_types.len() > 1
        && schema_types
            .iter()
            .any(|schema_type| matches_json_type(&value, schema_type));
    if !schema_types.is_empty() && !already_matches_union {
        for schema_type in &schema_types {
            let candidate = coerce_primitive_by_type(&value, schema_type);
            if candidate != value {
                value = candidate;
                break;
            }
        }
    }

    if schema_types.contains(&"object")
        && let Value::Object(object) = &mut value
    {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(properties) = properties {
            for (key, property_schema) in properties {
                if let Some(property_value) = object.remove(key) {
                    object.insert(
                        key.clone(),
                        coerce_with_json_schema(property_value, property_schema),
                    );
                }
            }
        }
        if let Some(additional_schema @ Value::Object(_)) = schema.get("additionalProperties") {
            let defined = properties
                .map(|properties| {
                    properties
                        .keys()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>()
                })
                .unwrap_or_default();
            for (key, property_value) in object.iter_mut() {
                if !defined.contains(key) {
                    *property_value =
                        coerce_with_json_schema(property_value.clone(), additional_schema);
                }
            }
        }
    }

    if schema_types.contains(&"array")
        && let Value::Array(values) = &mut value
    {
        match schema.get("items") {
            Some(Value::Array(item_schemas)) => {
                for (item, item_schema) in values.iter_mut().zip(item_schemas) {
                    *item = coerce_with_json_schema(item.clone(), item_schema);
                }
            }
            Some(item_schema @ Value::Object(_)) => {
                for item in values {
                    *item = coerce_with_json_schema(item.clone(), item_schema);
                }
            }
            Some(_) | None => {}
        }
    }

    value
}

fn coerce_with_union_schema(value: Value, schemas: &[Value]) -> Value {
    if schemas
        .iter()
        .any(|schema| sub_schema_accepts(schema, &value) == Some(true))
    {
        return value;
    }
    for schema in schemas {
        let candidate = coerce_with_json_schema(value.clone(), schema);
        if sub_schema_accepts(schema, &candidate) == Some(true) {
            return candidate;
        }
    }
    value
}

fn sub_schema_accepts(schema: &Value, value: &Value) -> Option<bool> {
    jsonschema::validator_for(schema)
        .ok()
        .map(|validator| validator.is_valid(value))
}

fn schema_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        Some(_) | None => Vec::new(),
    }
}

fn matches_json_type(value: &Value, schema_type: &str) -> bool {
    match schema_type {
        "number" => value.is_number(),
        "integer" => value.as_number().is_some_and(number_is_integer),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn number_is_integer(number: &serde_json::Number) -> bool {
    number.is_i64()
        || number.is_u64()
        || number.as_f64().is_some_and(|number| number.fract() == 0.0)
}

fn coerce_primitive_by_type(value: &Value, schema_type: &str) -> Value {
    match schema_type {
        "number" => match value {
            Value::Null => js_number_value(0.0).unwrap_or_else(|| value.clone()),
            Value::String(text) if !text.trim().is_empty() => {
                parse_js_number(text).unwrap_or_else(|| value.clone())
            }
            Value::Bool(boolean) => {
                js_number_value(if *boolean { 1.0 } else { 0.0 }).unwrap_or_else(|| value.clone())
            }
            _ => value.clone(),
        },
        "integer" => match value {
            Value::Null => Value::from(0),
            Value::String(text) if !text.trim().is_empty() => parse_js_number(text)
                .filter(|number| number.as_number().is_some_and(number_is_integer))
                .unwrap_or_else(|| value.clone()),
            Value::Bool(boolean) => Value::from(u8::from(*boolean)),
            _ => value.clone(),
        },
        "boolean" => match value {
            Value::Null => Value::Bool(false),
            Value::String(text) if text == "true" => Value::Bool(true),
            Value::String(text) if text == "false" => Value::Bool(false),
            Value::Number(number) if number.as_f64() == Some(1.0) => Value::Bool(true),
            Value::Number(number) if number.as_f64() == Some(0.0) => Value::Bool(false),
            _ => value.clone(),
        },
        "string" => match value {
            Value::Null => Value::String(String::new()),
            Value::Bool(boolean) => Value::String(boolean.to_string()),
            Value::Number(number) => Value::String(js_number_to_string(number)),
            _ => value.clone(),
        },
        "null" => match value {
            Value::String(text) if text.is_empty() => Value::Null,
            Value::Number(number) if number.as_f64().is_some_and(|number| number == 0.0) => {
                Value::Null
            }
            Value::Bool(false) => Value::Null,
            _ => value.clone(),
        },
        _ => value.clone(),
    }
}

fn parse_js_number(text: &str) -> Option<Value> {
    let text = text.trim_matches(is_ecmascript_whitespace);
    let integer = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        parse_radix_js_number(hex, 16)
    } else if let Some(binary) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
        parse_radix_js_number(binary, 2)
    } else if let Some(octal) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O")) {
        parse_radix_js_number(octal, 8)
    } else {
        None
    };
    if let Some(number) = integer {
        return js_number_value(number);
    }
    let parsed = text
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())?;
    js_number_value(parsed)
}

fn parse_radix_js_number(digits: &str, radix: u32) -> Option<f64> {
    BigUint::parse_bytes(digits.as_bytes(), radix)?.to_f64()
}

fn is_ecmascript_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn js_number_value(number: f64) -> Option<Value> {
    if number == 0.0 && number.is_sign_negative() {
        return serde_json::Number::from_f64(number).map(Value::Number);
    }
    if number.fract() == 0.0 {
        // The integer bounds are exclusive because `u64::MAX as f64` and
        // `i64::MAX as f64` round upward. Casting those rounded boundary values
        // would manufacture an integer different from JavaScript's Number.
        if (0.0..18_446_744_073_709_551_616.0).contains(&number) {
            return Some(Value::from(number as u64));
        }
        if (-9_223_372_036_854_775_808.0..0.0).contains(&number) {
            return Some(Value::from(number as i64));
        }
    }
    serde_json::Number::from_f64(number).map(Value::Number)
}

fn js_number_to_string(number: &serde_json::Number) -> String {
    let Some(number) = number.as_f64() else {
        return number.to_string();
    };
    ryu_js::Buffer::new().format(number).to_owned()
}
