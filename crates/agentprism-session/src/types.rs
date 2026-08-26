//! Durable session entries, operational records, mutations, and repository values.

use crate::{EntryId, LaneName, OperationRecordId, Sequence, SessionId};
use agentprism_ai::{
    AssistantFinishReason, Cost, ModelRef, PublicError, ReasoningLevel, RunId, Timestamp,
    ToolCallId, Usage, VersionedExtension,
};
use agentprism_core::AgentRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Current native session-header schema.
pub const SESSION_HEADER_SCHEMA_VERSION: u32 = 1;

/// Current derived session-state schema.
pub const SESSION_STATE_SCHEMA_VERSION: u32 = 1;

/// Current metadata schema.
pub const SESSION_METADATA_SCHEMA_VERSION: u32 = 1;

/// Current append-receipt schema.
pub const APPEND_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Current tail-repair report schema.
pub const TAIL_REPAIR_REPORT_SCHEMA_VERSION: u32 = 1;

/// Versioned environment metadata captured when a session is created.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionEnvironmentMetadata {
    /// Persistence schema for this environment value.
    pub schema_version: u32,
    /// Host working directory, when the host exposes one.
    pub working_directory: Option<String>,
    /// Namespaced, versioned host metadata not interpreted by this crate.
    #[serde(default)]
    pub extensions: BTreeMap<String, VersionedExtension>,
}

impl Default for SessionEnvironmentMetadata {
    fn default() -> Self {
        Self {
            schema_version: 1,
            working_directory: None,
            extensions: BTreeMap::new(),
        }
    }
}

/// Versioned immutable header for one native session log.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Native session schema.
    pub schema_version: u32,
    /// Stable session identifier.
    pub session_id: SessionId,
    /// Creation time.
    pub created_at: Timestamp,
    /// Source session when this session was forked.
    pub parent_session_id: Option<SessionId>,
    /// Host environment metadata captured at creation.
    pub environment: SessionEnvironmentMetadata,
}

impl SessionHeader {
    /// Creates a native version-one session header.
    pub fn new(
        session_id: impl Into<SessionId>,
        created_at: Timestamp,
        environment: SessionEnvironmentMetadata,
    ) -> Self {
        Self {
            schema_version: SESSION_HEADER_SCHEMA_VERSION,
            session_id: session_id.into(),
            created_at,
            parent_session_id: None,
            environment,
        }
    }
}

/// Fields shared by every immutable entry-tree node.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EntryBase {
    /// Stable entry identifier.
    pub id: EntryId,
    /// Session-global append sequence.
    pub sequence: Sequence,
    /// Earlier entry on this branch, or `None` at a root.
    pub parent_id: Option<EntryId>,
    /// Storage timestamp.
    pub timestamp: Timestamp,
}

/// One immutable branch-tree entry from Architecture v2 part 2 §7.2.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "Architecture v2 part 2 §7.2 specifies SessionEntry::Message with AgentRecord directly"
)]
pub enum SessionEntry {
    /// Durable agent transcript record.
    Message {
        /// Shared entry fields.
        base: EntryBase,
        /// Provider-neutral or custom agent record.
        message: AgentRecord,
        /// Tool result requested automatic run termination.
        terminate: bool,
    },
    /// Provider/model selection changed.
    ModelChange {
        /// Shared entry fields.
        base: EntryBase,
        /// Newly selected model.
        model: ModelRef,
    },
    /// Reasoning level changed.
    ReasoningChange {
        /// Shared entry fields.
        base: EntryBase,
        /// Newly selected reasoning level.
        level: ReasoningLevel,
    },
    /// Active tool set changed.
    ActiveToolsChange {
        /// Shared entry fields.
        base: EntryBase,
        /// Tool names active after this entry.
        tool_names: Vec<String>,
    },
    /// Durable compaction result.
    Compaction {
        /// Shared entry fields.
        base: EntryBase,
        /// Summary replacing the compacted prefix.
        summary: String,
        /// Uncompacted transcript tail.
        retained_tail: Vec<AgentRecord>,
        /// Estimated token count before compaction.
        tokens_before: u64,
        /// Policy-owned versioned details.
        details: Option<VersionedExtension>,
        /// Model usage incurred by summarization.
        usage: Option<Usage>,
    },
    /// Durable summary of an abandoned branch segment.
    BranchSummary {
        /// Shared entry fields.
        base: EntryBase,
        /// Navigation source leaf whose abandoned side was summarized.
        from_id: EntryId,
        /// Summary text.
        summary: String,
        /// Policy-owned versioned details.
        details: Option<VersionedExtension>,
        /// Model usage incurred by summarization.
        usage: Option<Usage>,
    },
    /// Application-defined tree entry.
    Custom {
        /// Shared entry fields.
        base: EntryBase,
        /// Open custom entry kind.
        custom_type: String,
        /// Versioned custom payload.
        data: Option<VersionedExtension>,
    },
}

impl SessionEntry {
    /// Returns the common entry fields.
    pub fn base(&self) -> &EntryBase {
        match self {
            Self::Message { base, .. }
            | Self::ModelChange { base, .. }
            | Self::ReasoningChange { base, .. }
            | Self::ActiveToolsChange { base, .. }
            | Self::Compaction { base, .. }
            | Self::BranchSummary { base, .. }
            | Self::Custom { base, .. } => base,
        }
    }

    /// Returns the stable entry identifier.
    pub fn id(&self) -> &EntryId {
        &self.base().id
    }

    /// Returns the session-global sequence.
    pub fn sequence(&self) -> Sequence {
        self.base().sequence
    }

    /// Returns the parent entry identifier.
    pub fn parent_id(&self) -> Option<&EntryId> {
        self.base().parent_id.as_ref()
    }

    /// Replaces storage-assigned base fields while retaining entry content.
    pub fn with_base(mut self, base: EntryBase) -> Self {
        match &mut self {
            Self::Message { base: target, .. }
            | Self::ModelChange { base: target, .. }
            | Self::ReasoningChange { base: target, .. }
            | Self::ActiveToolsChange { base: target, .. }
            | Self::Compaction { base: target, .. }
            | Self::BranchSummary { base: target, .. }
            | Self::Custom { base: target, .. } => *target = base,
        }
        self
    }
}

/// Entry content provisioned before a lane parent, sequence, and timestamp are assigned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "provisioned entries preserve the direct SessionEntry field shapes before base assignment"
)]
pub enum ProvisionedEntry {
    /// Durable agent transcript record.
    Message {
        /// Stable preallocated entry identifier.
        id: EntryId,
        /// Provider-neutral or custom agent record.
        message: AgentRecord,
        /// Tool result requested automatic run termination.
        terminate: bool,
    },
    /// Provider/model selection changed.
    ModelChange {
        /// Stable preallocated entry identifier.
        id: EntryId,
        /// Newly selected model.
        model: ModelRef,
    },
    /// Reasoning level changed.
    ReasoningChange {
        /// Stable preallocated entry identifier.
        id: EntryId,
        /// Newly selected reasoning level.
        level: ReasoningLevel,
    },
    /// Active tool set changed.
    ActiveToolsChange {
        /// Stable preallocated entry identifier.
        id: EntryId,
        /// Tool names active after this entry.
        tool_names: Vec<String>,
    },
    /// Durable compaction result.
    Compaction {
        /// Stable preallocated entry identifier.
        id: EntryId,
        /// Summary replacing the compacted prefix.
        summary: String,
        /// Uncompacted transcript tail.
        retained_tail: Vec<AgentRecord>,
        /// Estimated token count before compaction.
        tokens_before: u64,
        /// Policy-owned versioned details.
        details: Option<VersionedExtension>,
        /// Model usage incurred by summarization.
        usage: Option<Usage>,
    },
    /// Durable summary of an abandoned branch segment.
    BranchSummary {
        /// Stable preallocated entry identifier.
        id: EntryId,
        /// Navigation source leaf whose abandoned side was summarized.
        from_id: EntryId,
        /// Summary text.
        summary: String,
        /// Policy-owned versioned details.
        details: Option<VersionedExtension>,
        /// Model usage incurred by summarization.
        usage: Option<Usage>,
    },
    /// Application-defined tree entry.
    Custom {
        /// Stable preallocated entry identifier.
        id: EntryId,
        /// Open custom entry kind.
        custom_type: String,
        /// Versioned custom payload.
        data: Option<VersionedExtension>,
    },
}

impl ProvisionedEntry {
    /// Returns the preallocated entry identifier.
    pub fn id(&self) -> &EntryId {
        match self {
            Self::Message { id, .. }
            | Self::ModelChange { id, .. }
            | Self::ReasoningChange { id, .. }
            | Self::ActiveToolsChange { id, .. }
            | Self::Compaction { id, .. }
            | Self::BranchSummary { id, .. }
            | Self::Custom { id, .. } => id,
        }
    }

    /// Assigns immutable tree fields and returns a durable entry.
    pub fn materialize(
        self,
        sequence: Sequence,
        parent_id: Option<EntryId>,
        timestamp: Timestamp,
    ) -> SessionEntry {
        let base = EntryBase {
            id: self.id().clone(),
            sequence,
            parent_id,
            timestamp,
        };
        match self {
            Self::Message {
                message, terminate, ..
            } => SessionEntry::Message {
                base,
                message,
                terminate,
            },
            Self::ModelChange { model, .. } => SessionEntry::ModelChange { base, model },
            Self::ReasoningChange { level, .. } => SessionEntry::ReasoningChange { base, level },
            Self::ActiveToolsChange { tool_names, .. } => {
                SessionEntry::ActiveToolsChange { base, tool_names }
            }
            Self::Compaction {
                summary,
                retained_tail,
                tokens_before,
                details,
                usage,
                ..
            } => SessionEntry::Compaction {
                base,
                summary,
                retained_tail,
                tokens_before,
                details,
                usage,
            },
            Self::BranchSummary {
                from_id,
                summary,
                details,
                usage,
                ..
            } => SessionEntry::BranchSummary {
                base,
                from_id,
                summary,
                details,
                usage,
            },
            Self::Custom {
                custom_type, data, ..
            } => SessionEntry::Custom {
                base,
                custom_type,
                data,
            },
        }
    }
}

/// Fields shared by lane-scoped operational records.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationRecordBase {
    /// Stable record identifier.
    pub id: OperationRecordId,
    /// Session-global append sequence.
    pub sequence: Sequence,
    /// Lane that owns the operation.
    pub lane: LaneName,
    /// Storage timestamp.
    pub timestamp: Timestamp,
}

/// Persisted caller intent needed to resume an interrupted operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationIntent {
    /// Run the agent from normalized caller input.
    Run {
        /// Normalized prompt before run hooks.
        original_prompt: Vec<AgentRecord>,
        /// Provisioned initial entries in durable insertion order.
        initial_messages: Vec<ProvisionedEntry>,
        /// Optional system prompt override.
        system_prompt_override: Option<String>,
        /// Versioned extension data needed by resume hooks.
        #[serde(default)]
        resume_data: BTreeMap<String, VersionedExtension>,
    },
    /// Compact the current branch.
    Compaction {
        /// Optional caller instructions.
        custom_instructions: Option<String>,
        /// Preallocated result entry identifier.
        result_entry_id: EntryId,
    },
    /// Navigate to another tree position.
    Navigation {
        /// Target entry, or the empty root.
        target_id: Option<EntryId>,
        /// Whether the abandoned segment must be summarized.
        summarize: bool,
        /// Optional summary instructions.
        custom_instructions: Option<String>,
        /// Optional global label to set.
        label: Option<String>,
        /// Preallocated branch-summary entry when summarization is requested.
        summary_entry_id: Option<EntryId>,
    },
}

/// Durable terminal outcome of a harness operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationOutcome {
    /// Operation completed successfully.
    Completed,
    /// Operation was aborted.
    Aborted,
    /// Operation failed.
    Failed,
    /// A policy or user declined the operation.
    Declined,
}

/// Durable step kind inside an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStep {
    /// Assistant model request.
    Assistant,
    /// Compaction summary request.
    Compaction,
    /// Abandoned-branch summary request.
    BranchSummary,
}

/// Why compaction summary generation started.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    /// Explicit caller request.
    Manual,
    /// Configured context threshold.
    Threshold,
    /// Recovery from provider context overflow.
    Overflow,
}

/// Stable semantic identity of one assistant tool call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCallIdentity {
    /// Canonical call identifier.
    pub id: ToolCallId,
    /// Tool name selected by the assistant.
    pub name: String,
}

/// Whether recovery may safely replay a started tool call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReplayPolicy {
    /// Never invoke the call again automatically.
    Never,
    /// Tool contract declares replay safe.
    Safe,
}

/// Durable agent-input queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueKind {
    /// Steering input for an active run.
    Steer,
    /// Follow-up input consumed only when the run would stop.
    FollowUp,
    /// Input held for the next run.
    NextRun,
}

/// Attribution attached to a durable usage ledger record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum UsageAttribution {
    /// Assistant model request.
    Assistant {
        /// Owning run.
        run_id: RunId,
        /// Result entry.
        entry_id: EntryId,
        /// One-based logical attempt.
        attempt: u32,
        /// Terminal assistant reason.
        stop_reason: AssistantFinishReason,
    },
    /// Compaction summary request.
    Compaction {
        /// Owning run.
        run_id: RunId,
        /// Result entry.
        entry_id: EntryId,
        /// One-based logical attempt.
        attempt: u32,
        /// Terminal assistant reason.
        stop_reason: AssistantFinishReason,
    },
    /// Branch summary request.
    BranchSummary {
        /// Owning run.
        run_id: RunId,
        /// Result entry.
        entry_id: EntryId,
        /// One-based logical attempt.
        attempt: u32,
        /// Terminal assistant reason.
        stop_reason: AssistantFinishReason,
    },
    /// Fetch of a provider-deferred response.
    DeferredFetch {
        /// Owning run.
        run_id: RunId,
        /// Result entry.
        entry_id: EntryId,
        /// One-based logical attempt.
        attempt: u32,
        /// Terminal assistant reason.
        stop_reason: AssistantFinishReason,
    },
    /// Tool execution.
    Tool {
        /// Owning run.
        run_id: RunId,
        /// Result entry.
        entry_id: EntryId,
        /// Canonical tool call.
        tool_call_id: ToolCallId,
    },
    /// Harness hook execution.
    Hook {
        /// Owning run.
        run_id: RunId,
        /// Result entry.
        entry_id: EntryId,
    },
    /// Ledger correction not attributable to one ordinary step.
    Adjustment {
        /// Optional owning run.
        run_id: Option<RunId>,
        /// Optional related entry.
        entry_id: Option<EntryId>,
        /// Structured reason or provider detail.
        details: Option<Value>,
    },
}

/// Signed correction applied in addition to an unsigned canonical [`Usage`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedUsageAdjustment {
    /// Non-cache input token correction.
    pub input_tokens: i128,
    /// Output token correction.
    pub output_tokens: i128,
    /// Cache-read token correction.
    pub cache_read_tokens: i128,
    /// Cache-write token correction.
    pub cache_write_tokens: i128,
    /// Authoritative total-token correction.
    pub total_tokens: i128,
    /// Optional fixed-point monetary correction.
    pub cost: Option<Cost>,
}

/// Lane-scoped operational record from Architecture v2 part 2 §7.2.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationRecord {
    /// Durable operation start and resumable intent.
    Started {
        /// Shared record fields; its `id` is the operation identity.
        base: OperationRecordBase,
        /// Lane leaf observed before the operation began.
        source_leaf_id: Option<EntryId>,
        /// Caller intent required for recovery.
        intent: OperationIntent,
    },
    /// Cancellation request for an active operation.
    AbortRequested {
        /// Shared record fields.
        base: OperationRecordBase,
        /// Operation identity.
        run_id: RunId,
    },
    /// Durable operation terminal.
    Finished {
        /// Shared record fields.
        base: OperationRecordBase,
        /// Operation identity.
        run_id: RunId,
        /// Terminal outcome.
        outcome: OperationOutcome,
        /// Sanitized terminal error.
        error: Option<PublicError>,
    },
    /// One model-backed step attempt.
    StepAttempt {
        /// Shared record fields.
        base: OperationRecordBase,
        /// Owning operation.
        run_id: RunId,
        /// Step kind.
        step: OperationStep,
        /// One-based attempt number.
        attempt: u32,
        /// Preallocated result entry.
        result_entry_id: EntryId,
        /// Required only for compaction attempts.
        compaction_reason: Option<CompactionReason>,
    },
    /// Tool invocation began.
    ToolStarted {
        /// Shared record fields.
        base: OperationRecordBase,
        /// Owning operation.
        run_id: RunId,
        /// Assistant entry containing the call.
        assistant_entry_id: EntryId,
        /// Stable zero-based tool position within that assistant entry.
        tool_index: u32,
        /// Stable call identity.
        call: ToolCallIdentity,
        /// Effective validated arguments.
        effective_args: Value,
        /// Preallocated tool-result entry.
        result_entry_id: EntryId,
        /// Crash-replay policy.
        replay: ToolReplayPolicy,
    },
    /// Input was durably enqueued.
    QueueEnqueued {
        /// Shared record fields.
        base: OperationRecordBase,
        /// Owning operation for active-run queues.
        run_id: Option<RunId>,
        /// Queue kind.
        queue: QueueKind,
        /// Preallocated target entry.
        target: ProvisionedEntry,
    },
    /// A provisioned queued entry was cancelled.
    QueueCancelled {
        /// Shared record fields.
        base: OperationRecordBase,
        /// Owning operation when applicable.
        run_id: Option<RunId>,
        /// Preallocated queued entry identifier.
        entry_id: EntryId,
    },
    /// An entry write was deferred until after the active mutation boundary.
    WriteDeferred {
        /// Shared record fields.
        base: OperationRecordBase,
        /// Owning operation.
        run_id: RunId,
        /// Preallocated target entry.
        target: ProvisionedEntry,
    },
    /// Usage and optional fixed-point cost ledger item.
    Usage {
        /// Shared record fields.
        base: OperationRecordBase,
        /// Semantic usage attribution.
        attribution: UsageAttribution,
        /// Unsigned canonical usage.
        usage: Usage,
        /// Separately retained fixed-point cost.
        cost: Option<Cost>,
        /// Signed correction used only when importing or recording adjustments.
        adjustment: Option<SignedUsageAdjustment>,
    },
}

impl OperationRecord {
    /// Returns the common record fields.
    pub fn base(&self) -> &OperationRecordBase {
        match self {
            Self::Started { base, .. }
            | Self::AbortRequested { base, .. }
            | Self::Finished { base, .. }
            | Self::StepAttempt { base, .. }
            | Self::ToolStarted { base, .. }
            | Self::QueueEnqueued { base, .. }
            | Self::QueueCancelled { base, .. }
            | Self::WriteDeferred { base, .. }
            | Self::Usage { base, .. } => base,
        }
    }

    /// Returns the record sequence.
    pub fn sequence(&self) -> Sequence {
        self.base().sequence
    }

    /// Returns the owning lane.
    pub fn lane(&self) -> &LaneName {
        &self.base().lane
    }

    /// Returns the referenced operation identity when this record has one.
    pub fn run_id(&self) -> Option<RunId> {
        match self {
            Self::Started { base, .. } => Some(RunId::new(base.id.as_str())),
            Self::AbortRequested { run_id, .. }
            | Self::Finished { run_id, .. }
            | Self::StepAttempt { run_id, .. }
            | Self::ToolStarted { run_id, .. }
            | Self::WriteDeferred { run_id, .. } => Some(run_id.clone()),
            Self::QueueEnqueued { run_id, .. } | Self::QueueCancelled { run_id, .. } => {
                run_id.clone()
            }
            Self::Usage { attribution, .. } => attribution.run_id().cloned(),
        }
    }
}

impl UsageAttribution {
    /// Returns the owning run when the attribution has one.
    pub fn run_id(&self) -> Option<&RunId> {
        match self {
            Self::Assistant { run_id, .. }
            | Self::Compaction { run_id, .. }
            | Self::BranchSummary { run_id, .. }
            | Self::DeferredFetch { run_id, .. }
            | Self::Tool { run_id, .. }
            | Self::Hook { run_id, .. } => Some(run_id),
            Self::Adjustment { run_id, .. } => run_id.as_ref(),
        }
    }
}

/// Global session fact whose latest mutation wins.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case")]
pub enum SessionFact {
    /// Session display name.
    Name {
        /// New name, or `None` to clear it.
        name: Option<String>,
    },
    /// Entry label.
    Label {
        /// Globally addressed entry.
        target_id: EntryId,
        /// New label, or `None` to clear it.
        label: Option<String>,
    },
}

/// One item in the authoritative native append log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionMutation {
    /// Immutable tree entry, optionally bound to and advancing a lane.
    Entry {
        /// Lane advanced by this append; absent for imported tree entries.
        lane: Option<LaneName>,
        /// Complete immutable entry.
        entry: SessionEntry,
    },
    /// Lane-scoped operational record.
    Record {
        /// Complete operational record.
        record: OperationRecord,
    },
    /// Create or move a durable lane pointer.
    Lane {
        /// Session-global append sequence.
        sequence: Sequence,
        /// Lane to create or move.
        lane: LaneName,
        /// Existing target entry, or the empty root.
        leaf_id: Option<EntryId>,
    },
    /// Update a global latest-value fact.
    Fact {
        /// Session-global append sequence.
        sequence: Sequence,
        /// Fact update.
        fact: SessionFact,
    },
}

impl SessionMutation {
    /// Returns this mutation's session-global sequence.
    pub fn sequence(&self) -> Sequence {
        match self {
            Self::Entry { entry, .. } => entry.sequence(),
            Self::Record { record } => record.sequence(),
            Self::Lane { sequence, .. } | Self::Fact { sequence, .. } => *sequence,
        }
    }
}

/// Current pointer for a named branch lane.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaneState {
    /// Lane name.
    pub name: LaneName,
    /// Current immutable entry-tree leaf.
    pub leaf_id: Option<EntryId>,
}

/// Position copied by a repository-level session fork.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "position", content = "entry_id", rename_all = "snake_case")]
pub enum ForkPosition {
    /// Copy the branch prefix ending immediately before this entry.
    Before(EntryId),
    /// Copy the branch prefix through this entry.
    At(EntryId),
    /// Copy the complete immutable tree and all lane pointers.
    WholeTree,
}

/// Recovery classification for one selected lane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum RecoveryDecision {
    /// No unresolved operation exists.
    Idle,
    /// Exactly one operation can be resumed.
    Resume {
        /// Open operation-start record.
        operation: OperationRecord,
        /// Later durable records belonging to the operation.
        completed_steps: Vec<OperationRecord>,
    },
    /// An abort request makes the open operation explicitly abandonable.
    Abandon {
        /// Open operation-start record.
        operation: OperationRecord,
        /// Sanitized recovery reason.
        reason: PublicError,
    },
    /// More than one unresolved operation exists on the lane.
    Corrupt {
        /// Conflicting operation-start records, newest first.
        open_operations: Vec<OperationRecord>,
    },
}

/// Aggregate values derived only from durable entries and usage records.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    /// Number of message entries across the complete tree.
    pub message_count: u64,
    /// Cache-read token ledger total.
    pub cached_tokens: i128,
    /// Non-cache input plus cache-write token ledger total.
    pub uncached_tokens: i128,
    /// Authoritative token ledger total.
    pub total_tokens: i128,
    /// Fixed-point totals separated by currency code.
    pub cost_micros_by_currency: BTreeMap<String, i128>,
}

/// Metadata returned without exposing storage implementation details.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Metadata schema.
    pub schema_version: u32,
    /// Stable session identifier.
    pub session_id: SessionId,
    /// Creation time.
    pub created_at: Timestamp,
    /// Parent session for a repository fork.
    pub parent_session_id: Option<SessionId>,
    /// Captured environment metadata.
    pub environment: SessionEnvironmentMetadata,
    /// Last accepted global sequence.
    pub last_sequence: Sequence,
}

/// Atomic append result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppendReceipt {
    /// Receipt schema.
    pub schema_version: u32,
    /// Sequence observed before this batch.
    pub previous_sequence: Sequence,
    /// Sequence after this batch.
    pub last_sequence: Sequence,
    /// Number of accepted mutations.
    pub mutation_count: usize,
}

/// Backend-specific torn-tail repair result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TailRepairReport {
    /// Report schema.
    pub schema_version: u32,
    /// Whether any durable bytes or records were repaired.
    pub repaired: bool,
    /// Number of removed bytes when known.
    pub removed_bytes: u64,
    /// Last valid session sequence after repair.
    pub last_sequence: Sequence,
}

/// Request for creation of an empty native session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    /// Stable new session identifier.
    pub session_id: SessionId,
    /// Creation time supplied by the host clock.
    pub created_at: Timestamp,
    /// Optional parent supplied for imports or application-defined lineage.
    pub parent_session_id: Option<SessionId>,
    /// Captured environment metadata.
    pub environment: SessionEnvironmentMetadata,
}

impl CreateSessionRequest {
    /// Creates a request for a root session.
    pub fn new(
        session_id: impl Into<SessionId>,
        created_at: Timestamp,
        environment: SessionEnvironmentMetadata,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            created_at,
            parent_session_id: None,
            environment,
        }
    }

    /// Converts the request into its immutable header.
    pub fn into_header(self) -> SessionHeader {
        SessionHeader {
            schema_version: SESSION_HEADER_SCHEMA_VERSION,
            session_id: self.session_id,
            created_at: self.created_at,
            parent_session_id: self.parent_session_id,
            environment: self.environment,
        }
    }
}

/// Request for a repository-level immutable session fork.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForkRequest {
    /// Stable destination session identifier.
    pub session_id: SessionId,
    /// Destination creation time.
    pub created_at: Timestamp,
    /// Destination environment metadata.
    pub environment: SessionEnvironmentMetadata,
    /// Branch prefix or complete tree to copy.
    pub position: ForkPosition,
}

/// Repository listing filter.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionQuery {
    /// Restrict results to sessions forked from this parent.
    pub parent_session_id: Option<SessionId>,
    /// Positive result cap.
    pub limit: Option<usize>,
}
