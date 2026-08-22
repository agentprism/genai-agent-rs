//! Ordered snapshot restoration from Architecture v2 part 1 §4.9.

use crate::{
    AGENT_INITIAL_SEQUENCE, AGENT_SNAPSHOT_SCHEMA_VERSION, AGENT_STATE_SCHEMA_VERSION,
    AgentControl, AgentDefaults, AgentError, AgentRecord, AgentSnapshot, AgentState,
    DefaultContextPolicy, DefaultTurnPolicy, LocalAgentDefaults, LocalContextPolicy,
    LocalToolRegistry, LocalToolScheduler, LocalTurnPolicy, QueueReceiver, ToolExecutionMode,
    ToolRegistry, ToolScheduler,
};
use pi_ai::{
    AssistantMessageSnapshot, LocalModelRuntime, ModelRef, ModelRuntime, PublicError,
    SimpleGenerationOptions, ToolCallId,
};
use std::{collections::BTreeSet, rc::Rc, sync::Arc};

/// Synchronous catalog capability used only to validate a persisted model
/// reference during restoration.
///
/// It is intentionally narrower than both `Models` and [`ModelRuntime`].
pub trait ModelRefResolver {
    /// Returns whether the current application catalog resolves this model.
    fn resolves(&self, model: &ModelRef) -> bool;
}

impl<F> ModelRefResolver for F
where
    F: Fn(&ModelRef) -> bool,
{
    fn resolves(&self, model: &ModelRef) -> bool {
        self(model)
    }
}

/// Registry capability used to validate persisted custom record kinds.
pub trait CustomRecordKindRegistry {
    /// Returns whether this process can interpret the custom kind.
    fn contains(&self, type_name: &str) -> bool;
}

impl<F> CustomRecordKindRegistry for F
where
    F: Fn(&str) -> bool,
{
    fn contains(&self, type_name: &str) -> bool {
        self(type_name)
    }
}

/// Explicit set of custom record kinds accepted during restoration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CustomRecordKinds {
    names: BTreeSet<String>,
}

impl CustomRecordKinds {
    /// Creates an empty registry, which accepts no custom records.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one non-empty custom kind.
    pub fn register(&mut self, type_name: impl Into<String>) -> Result<(), AgentError> {
        let type_name = type_name.into();
        if type_name.is_empty() {
            return Err(AgentError::InvalidConfiguration {
                message: "custom agent record kind must not be empty".into(),
            });
        }
        self.names.insert(type_name);
        Ok(())
    }
}

impl CustomRecordKindRegistry for CustomRecordKinds {
    fn contains(&self, type_name: &str) -> bool {
        self.names.contains(type_name)
    }
}

/// Migrates and validates the versioned snapshot envelope before dependency
/// resolution.
///
/// Version one is the initial native schema, so the current migration is an
/// identity operation with explicit rejection of unknown versions. Future
/// versions extend this function before any model or tool binding occurs.
pub fn migrate_agent_snapshot(snapshot: AgentSnapshot) -> Result<AgentSnapshot, AgentError> {
    if snapshot.schema_version != AGENT_SNAPSHOT_SCHEMA_VERSION {
        return Err(AgentError::UnsupportedSnapshotSchema {
            found: snapshot.schema_version,
            supported: AGENT_SNAPSHOT_SCHEMA_VERSION,
        });
    }
    if snapshot.state.schema_version != AGENT_STATE_SCHEMA_VERSION {
        return Err(AgentError::UnsupportedStateSchema {
            found: snapshot.state.schema_version,
            supported: AGENT_STATE_SCHEMA_VERSION,
        });
    }
    if snapshot.next_sequence < AGENT_INITIAL_SEQUENCE {
        return Err(AgentError::InvalidNextSequence {
            next_sequence: snapshot.next_sequence,
        });
    }
    Ok(snapshot)
}

fn validate_custom_records(
    state: &AgentState,
    custom_kinds: &dyn CustomRecordKindRegistry,
) -> Result<(), AgentError> {
    for record in &state.transcript {
        if let AgentRecord::Custom { type_name, .. } = record
            && !custom_kinds.contains(type_name)
        {
            return Err(AgentError::UnknownCustomRecordKind {
                type_name: type_name.clone(),
            });
        }
    }
    Ok(())
}

/// Send-capable stateful agent restored around an explicitly injected runtime.
///
/// M2.1 owns restoration and state observation; subsequent M2 packages add the
/// run state machine over these exact fields.
pub struct Agent {
    pub(crate) runtime: Arc<dyn ModelRuntime>,
    pub(crate) state: AgentState,
    pub(crate) tools: ToolRegistry,
    pub(crate) next_sequence: u64,
    pub(crate) streaming: Option<AssistantMessageSnapshot>,
    pub(crate) pending_tool_calls: Arc<[ToolCallId]>,
    pub(crate) control: AgentControl,
    pub(crate) queue_rx: QueueReceiver,
    pub(crate) active_run: Option<pi_ai::RunId>,
    pub(crate) phase: Option<crate::AgentPhase>,
    pub(crate) last_error: Option<PublicError>,
    pub(crate) options: SimpleGenerationOptions,
    pub(crate) tool_execution: ToolExecutionMode,
    pub(crate) tool_scheduler: ToolScheduler,
    pub(crate) context_policy: Arc<dyn crate::ContextPolicy>,
    pub(crate) turn_policy: Arc<dyn crate::TurnPolicy>,
    pub(crate) defaults: AgentDefaults,
    pub(crate) next_identity: u64,
}

impl Agent {
    /// Restores in the required order: migrate schema, resolve [`ModelRef`],
    /// bind and validate [`ToolRegistry`], validate custom kinds, construct.
    pub fn restore(
        snapshot: AgentSnapshot,
        runtime: Arc<dyn ModelRuntime>,
        models: &dyn ModelRefResolver,
        tools: ToolRegistry,
        custom_kinds: &dyn CustomRecordKindRegistry,
    ) -> Result<Self, AgentError> {
        let snapshot = migrate_agent_snapshot(snapshot)?;
        if !models.resolves(&snapshot.state.model) {
            return Err(AgentError::UnresolvedModel {
                model: snapshot.state.model,
            });
        }
        tools.validate()?;
        validate_custom_records(&snapshot.state, custom_kinds)?;
        let (control, queue_rx) = AgentControl::channel(crate::DEFAULT_QUEUE_CAPACITY);
        let defaults = AgentDefaults::new(&snapshot.state, &tools);
        Ok(Self {
            runtime,
            state: snapshot.state,
            tools,
            next_sequence: snapshot.next_sequence,
            streaming: snapshot.streaming,
            pending_tool_calls: snapshot.pending_tool_calls,
            control,
            queue_rx,
            active_run: None,
            phase: None,
            last_error: None,
            options: SimpleGenerationOptions::default(),
            tool_execution: ToolExecutionMode::Parallel,
            tool_scheduler: ToolScheduler::default(),
            context_policy: Arc::new(DefaultContextPolicy),
            turn_policy: Arc::new(DefaultTurnPolicy),
            defaults,
            next_identity: 1,
        })
    }

    /// Returns durable agent state.
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Returns the bound model execution capability.
    pub fn runtime(&self) -> &Arc<dyn ModelRuntime> {
        &self.runtime
    }

    /// Returns the bound executable tool registry.
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Returns a complete owned persistence and observation snapshot.
    pub fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            schema_version: AGENT_SNAPSHOT_SCHEMA_VERSION,
            state: self.state.clone(),
            next_sequence: self.next_sequence,
            streaming: self.streaming.clone(),
            pending_tool_calls: self.pending_tool_calls.clone(),
        }
    }
}

/// Local/WASM stateful agent restored around a non-`Send` runtime and tools.
pub struct LocalAgent {
    pub(crate) runtime: Rc<dyn LocalModelRuntime>,
    pub(crate) state: AgentState,
    pub(crate) tools: LocalToolRegistry,
    pub(crate) next_sequence: u64,
    pub(crate) streaming: Option<AssistantMessageSnapshot>,
    pub(crate) pending_tool_calls: Arc<[ToolCallId]>,
    pub(crate) control: AgentControl,
    pub(crate) queue_rx: QueueReceiver,
    pub(crate) active_run: Option<pi_ai::RunId>,
    pub(crate) phase: Option<crate::AgentPhase>,
    pub(crate) last_error: Option<PublicError>,
    pub(crate) options: SimpleGenerationOptions,
    pub(crate) tool_execution: ToolExecutionMode,
    pub(crate) tool_scheduler: LocalToolScheduler,
    pub(crate) context_policy: Rc<dyn LocalContextPolicy>,
    pub(crate) turn_policy: Rc<dyn LocalTurnPolicy>,
    pub(crate) defaults: LocalAgentDefaults,
    pub(crate) next_identity: u64,
}

impl LocalAgent {
    /// Restores the local trait family in the same ordered phases as
    /// [`Agent::restore`].
    pub fn restore(
        snapshot: AgentSnapshot,
        runtime: Rc<dyn LocalModelRuntime>,
        models: &dyn ModelRefResolver,
        tools: LocalToolRegistry,
        custom_kinds: &dyn CustomRecordKindRegistry,
    ) -> Result<Self, AgentError> {
        let snapshot = migrate_agent_snapshot(snapshot)?;
        if !models.resolves(&snapshot.state.model) {
            return Err(AgentError::UnresolvedModel {
                model: snapshot.state.model,
            });
        }
        tools.validate()?;
        validate_custom_records(&snapshot.state, custom_kinds)?;
        let (control, queue_rx) = AgentControl::channel(crate::DEFAULT_QUEUE_CAPACITY);
        let defaults = LocalAgentDefaults::new(&snapshot.state, &tools);
        Ok(Self {
            runtime,
            state: snapshot.state,
            tools,
            next_sequence: snapshot.next_sequence,
            streaming: snapshot.streaming,
            pending_tool_calls: snapshot.pending_tool_calls,
            control,
            queue_rx,
            active_run: None,
            phase: None,
            last_error: None,
            options: SimpleGenerationOptions::default(),
            tool_execution: ToolExecutionMode::Parallel,
            tool_scheduler: LocalToolScheduler::default(),
            context_policy: Rc::new(DefaultContextPolicy),
            turn_policy: Rc::new(DefaultTurnPolicy),
            defaults,
            next_identity: 1,
        })
    }

    /// Returns durable agent state.
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Returns the bound local model execution capability.
    pub fn runtime(&self) -> &Rc<dyn LocalModelRuntime> {
        &self.runtime
    }

    /// Returns the bound local executable tool registry.
    pub fn tools(&self) -> &LocalToolRegistry {
        &self.tools
    }

    /// Returns a complete owned persistence and observation snapshot.
    pub fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            schema_version: AGENT_SNAPSHOT_SCHEMA_VERSION,
            state: self.state.clone(),
            next_sequence: self.next_sequence,
            streaming: self.streaming.clone(),
            pending_tool_calls: self.pending_tool_calls.clone(),
        }
    }
}
