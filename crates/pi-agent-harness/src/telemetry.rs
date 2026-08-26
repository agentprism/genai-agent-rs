//! Versioned, content-excluding harness telemetry envelope.

use pi_agent_session::{AppendReceipt, CompactionReason, LaneName, Sequence, SessionId};
use pi_ai::{
    AssistantFinishReason, LocalBoxFuture, ModelRef, RunId, SendBoxFuture, Timestamp, Usage,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fmt, rc::Rc, sync::Arc, time::Duration};

/// Current telemetry envelope schema version.
pub const TELEMETRY_SCHEMA_VERSION: u32 = 1;

/// Stable open identifier for one telemetry event.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TelemetryEventId(String);

impl TelemetryEventId {
    /// Creates an event identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TelemetryEventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable durable harness-operation identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    /// Creates an operation identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Low-cardinality, content-free handoff report summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandoffTelemetrySummary {
    /// Number of distinct source model fingerprints.
    pub source_model_count: u32,
    /// Number of reported projection changes.
    pub change_count: u64,
    /// Whether any reported change was lossy.
    pub lossy: bool,
}

/// Published telemetry event vocabulary from Architecture v2 part 2 §7.12.
///
/// The vocabulary intentionally has no prompt, response, tool argument, tool
/// output, authentication, header, or replay-payload field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum TelemetryEvent {
    /// One admitted run began.
    RunStarted {
        /// Selected model.
        #[schemars(with = "TelemetryModelRefSchema")]
        model: ModelRef,
    },
    /// One logical model request attempt began.
    ModelRequestStarted {
        /// Zero-based transport or harness attempt number supplied by the caller.
        attempt: u32,
    },
    /// One logical model request completed.
    ModelRequestFinished {
        /// Normalized terminal reason.
        #[schemars(with = "AssistantFinishReasonSchema")]
        finish: AssistantFinishReason,
        /// Content-free cumulative token usage.
        #[schemars(with = "UsageSchema")]
        usage: Usage,
        /// Wall-clock request duration, serialized as integer milliseconds.
        #[serde(with = "duration_millis")]
        #[schemars(with = "u64")]
        duration: Duration,
    },
    /// One tool execution began.
    ToolStarted {
        /// Stable tool name; arguments are deliberately excluded.
        tool_name: String,
    },
    /// One tool execution completed.
    ToolFinished {
        /// Stable tool name; output is deliberately excluded.
        tool_name: String,
        /// Whether execution succeeded.
        success: bool,
        /// Wall-clock tool duration, serialized as integer milliseconds.
        #[serde(with = "duration_millis")]
        #[schemars(with = "u64")]
        duration: Duration,
    },
    /// Compaction summary generation began.
    CompactionStarted {
        /// Trigger for this compaction.
        #[schemars(with = "CompactionReasonSchema")]
        reason: CompactionReason,
    },
    /// Compaction summary generation completed.
    CompactionFinished {
        /// Content-free cumulative summary-model usage.
        #[schemars(with = "UsageSchema")]
        usage: Usage,
    },
    /// A session mutation was accepted by durable storage.
    SessionMutationCommitted {
        /// Low-cardinality mutation kind such as `entry` or `record`.
        mutation_kind: String,
    },
    /// A provider context handoff was performed.
    HandoffPerformed {
        /// Content-free report summary.
        report: HandoffTelemetrySummary,
    },
}

/// Versioned correlation envelope distinct from agent and session events.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryEnvelope {
    /// Required schema version.
    pub schema_version: u32,
    /// Stable telemetry event identity.
    #[schemars(with = "String")]
    pub event_id: TelemetryEventId,
    /// Wall-clock event timestamp in Unix milliseconds.
    #[schemars(with = "i64")]
    pub timestamp: Timestamp,
    /// Durable session correlation, when available.
    #[schemars(with = "Option<String>")]
    pub session_id: Option<SessionId>,
    /// Durable lane correlation, when available.
    #[schemars(with = "Option<String>")]
    pub lane: Option<LaneName>,
    /// Agent run correlation, when available.
    #[schemars(with = "Option<String>")]
    pub run_id: Option<RunId>,
    /// Durable harness-operation correlation, when available.
    #[schemars(with = "Option<String>")]
    pub operation_id: Option<OperationId>,
    /// Accepted session sequence, when available.
    #[schemars(with = "Option<u64>")]
    pub sequence: Option<Sequence>,
    /// Content-free event payload.
    pub event: TelemetryEvent,
}

impl TelemetryEnvelope {
    /// Creates an uncorrelated version-one envelope.
    pub fn new(event_id: TelemetryEventId, timestamp: Timestamp, event: TelemetryEvent) -> Self {
        Self {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            event_id,
            timestamp,
            session_id: None,
            lane: None,
            run_id: None,
            operation_id: None,
            sequence: None,
            event,
        }
    }

    /// Adds session, lane, run, and operation correlation.
    pub fn with_correlation(
        mut self,
        session_id: Option<SessionId>,
        lane: Option<LaneName>,
        run_id: Option<RunId>,
        operation_id: Option<OperationId>,
    ) -> Self {
        self.session_id = session_id;
        self.lane = lane;
        self.run_id = run_id;
        self.operation_id = operation_id;
        self
    }
}

/// Telemetry sink failure.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelemetryError {
    /// Sink rejected or could not deliver an event.
    Sink {
        /// Sanitized sink diagnostic.
        message: String,
    },
}

impl TelemetryError {
    /// Creates a sanitized sink error.
    pub fn sink(message: impl Into<String>) -> Self {
        Self::Sink {
            message: message.into(),
        }
    }
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sink { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TelemetryError {}

/// Send telemetry-delivery seam.
pub trait TelemetrySink: Send + Sync + 'static {
    /// Emits one fully constructed telemetry envelope.
    fn emit(&self, event: TelemetryEnvelope) -> SendBoxFuture<'_, Result<(), TelemetryError>>;
}

/// Single-threaded counterpart of [`TelemetrySink`].
pub trait LocalTelemetrySink: 'static {
    /// Emits one fully constructed telemetry envelope.
    fn emit(&self, event: TelemetryEnvelope) -> LocalBoxFuture<'_, Result<(), TelemetryError>>;
}

/// Whether sink failures are advisory or compliance failures.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TelemetryFailurePolicy {
    /// Ignore sink failures after the sink future settles.
    #[default]
    BestEffort,
    /// Propagate sink failures to the caller.
    Required,
}

/// Send telemetry dispatcher with explicit sink-failure policy.
#[derive(Clone)]
pub struct TelemetryEmitter {
    sink: Arc<dyn TelemetrySink>,
    failure_policy: TelemetryFailurePolicy,
}

impl TelemetryEmitter {
    /// Creates a best-effort dispatcher, the architecture default.
    pub fn new(sink: Arc<dyn TelemetrySink>) -> Self {
        Self {
            sink,
            failure_policy: TelemetryFailurePolicy::BestEffort,
        }
    }

    /// Creates a dispatcher with an explicit compliance policy.
    pub fn with_failure_policy(
        sink: Arc<dyn TelemetrySink>,
        failure_policy: TelemetryFailurePolicy,
    ) -> Self {
        Self {
            sink,
            failure_policy,
        }
    }

    /// Emits one event, swallowing failure only in best-effort mode.
    pub fn emit(&self, event: TelemetryEnvelope) -> SendBoxFuture<'_, Result<(), TelemetryError>> {
        Box::pin(async move {
            match self.sink.emit(event).await {
                Ok(()) => Ok(()),
                Err(_) if self.failure_policy == TelemetryFailurePolicy::BestEffort => Ok(()),
                Err(error) => Err(error),
            }
        })
    }

    /// Emits durable-mutation telemetry from an already accepted append receipt.
    ///
    /// Requiring the receipt in this API prevents emission before storage
    /// acceptance in the normal harness call path.
    pub fn emit_committed_mutation(
        &self,
        receipt: &AppendReceipt,
        mut envelope: TelemetryEnvelope,
        mutation_kind: impl Into<String>,
    ) -> SendBoxFuture<'_, Result<(), TelemetryError>> {
        envelope.sequence = Some(receipt.last_sequence);
        envelope.event = TelemetryEvent::SessionMutationCommitted {
            mutation_kind: mutation_kind.into(),
        };
        self.emit(envelope)
    }
}

/// Local telemetry dispatcher with explicit sink-failure policy.
#[derive(Clone)]
pub struct LocalTelemetryEmitter {
    sink: Rc<dyn LocalTelemetrySink>,
    failure_policy: TelemetryFailurePolicy,
}

impl LocalTelemetryEmitter {
    /// Creates a best-effort local dispatcher.
    pub fn new(sink: Rc<dyn LocalTelemetrySink>) -> Self {
        Self {
            sink,
            failure_policy: TelemetryFailurePolicy::BestEffort,
        }
    }

    /// Creates a local dispatcher with an explicit compliance policy.
    pub fn with_failure_policy(
        sink: Rc<dyn LocalTelemetrySink>,
        failure_policy: TelemetryFailurePolicy,
    ) -> Self {
        Self {
            sink,
            failure_policy,
        }
    }

    /// Emits one local event, swallowing failure only in best-effort mode.
    pub fn emit(&self, event: TelemetryEnvelope) -> LocalBoxFuture<'_, Result<(), TelemetryError>> {
        Box::pin(async move {
            match self.sink.emit(event).await {
                Ok(()) => Ok(()),
                Err(_) if self.failure_policy == TelemetryFailurePolicy::BestEffort => Ok(()),
                Err(error) => Err(error),
            }
        })
    }

    /// Emits local durable-mutation telemetry from an accepted append receipt.
    pub fn emit_committed_mutation(
        &self,
        receipt: &AppendReceipt,
        mut envelope: TelemetryEnvelope,
        mutation_kind: impl Into<String>,
    ) -> LocalBoxFuture<'_, Result<(), TelemetryError>> {
        envelope.sequence = Some(receipt.last_sequence);
        envelope.event = TelemetryEvent::SessionMutationCommitted {
            mutation_kind: mutation_kind.into(),
        };
        self.emit(envelope)
    }
}

/// Sink that intentionally discards all telemetry.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopTelemetrySink;

impl TelemetrySink for NoopTelemetrySink {
    fn emit(&self, _event: TelemetryEnvelope) -> SendBoxFuture<'_, Result<(), TelemetryError>> {
        Box::pin(async { Ok(()) })
    }
}

impl LocalTelemetrySink for NoopTelemetrySink {
    fn emit(&self, _event: TelemetryEnvelope) -> LocalBoxFuture<'_, Result<(), TelemetryError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Generates the JSON Schema for [`TelemetryEnvelope`] from its Rust type.
pub fn telemetry_json_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(TelemetryEnvelope))
        .expect("the telemetry schema is JSON serializable")
}

/// Generates the canonical checked-in JSON Schema representation.
pub fn telemetry_json_schema_pretty() -> String {
    let mut schema = serde_json::to_string_pretty(&telemetry_json_schema())
        .expect("the telemetry schema is JSON serializable");
    schema.push('\n');
    schema
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct TelemetryModelRefSchema {
    provider: String,
    model: String,
}

#[derive(JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum AssistantFinishReasonSchema {
    Stop,
    Length,
    ToolUse,
    Deferred,
    Error,
    Aborted,
}

#[derive(JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum UsageSourceSchema {
    ProviderReported,
    Estimated,
    Mixed,
    Unknown,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct UsageSchema {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    cache_write_one_hour_tokens: Option<u64>,
    total_tokens: Option<u64>,
    source: UsageSourceSchema,
}

#[derive(JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum CompactionReasonSchema {
    Manual,
    Threshold,
    Overflow,
}

mod duration_millis {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub(super) fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        serializer.serialize_u64(millis)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}
