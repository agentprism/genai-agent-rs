//! Agent lifecycle events and structured outcomes from Architecture v2 part 1
//! §4.4 and §7, revised by part 2 §2.1 and §4.4.

use crate::{AgentRecord, ToolOutput, ToolUpdate};
use pi_ai::{
    AssistantEvent, AssistantFinishReason, CancellationReason, Cost, HandoffReport, MessageId,
    ModelRef, PublicError, RunId, ToolCall, ToolCallId, Usage,
};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::value::RawValue;

/// Canonical or custom role associated with a message lifecycle event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// User-authored canonical message.
    User,
    /// Model-authored canonical message.
    Assistant,
    /// Canonical tool-result message.
    ToolResult,
    /// Application-defined custom record.
    Custom,
}

/// Immutable facts produced by one completed model turn.
///
/// The committed transcript remains authoritative. This outcome therefore
/// refers to committed records by stable ID rather than duplicating messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnOutcome {
    /// Committed assistant record that completed the turn.
    pub assistant_message_id: MessageId,
    /// Assistant terminal classification.
    pub assistant_finish: AssistantFinishReason,
    /// Committed tool-result records in assistant source order.
    pub tool_result_message_ids: Vec<MessageId>,
    /// Cumulative usage for this assistant response.
    pub usage: Usage,
    /// Calculated response cost when model pricing was available.
    pub cost: Option<Cost>,
}

/// Expected terminal result of one agent run.
///
/// Failure and cancellation identify assistant records already committed to
/// [`crate::AgentState::transcript`]; there is no separate uncommitted partial
/// message at the agent boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunOutcome {
    /// The run reached an ordinary or tool-terminated completion.
    Completed {
        /// Last committed assistant message.
        final_message_id: MessageId,
        /// Aggregated run usage.
        usage: Usage,
        /// Aggregated run cost when every required price was available.
        cost: Option<Cost>,
    },
    /// The provider request or established response stream failed.
    Failed {
        /// Failed assistant message already committed to the transcript.
        committed_message_id: MessageId,
        /// Sanitized structured failure carried by that record.
        error: PublicError,
    },
    /// The caller cancelled the model or tool work.
    Cancelled {
        /// Aborted assistant message already committed to the transcript.
        committed_message_id: MessageId,
        /// Portable cancellation reason.
        reason: CancellationReason,
    },
}

/// Ordered state-machine observation emitted by the low-level agent run.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Begins one run.
    RunStarted {
        /// Stable run identity.
        run_id: RunId,
    },
    /// Begins one model turn within the run.
    TurnStarted {
        /// Stable run identity.
        run_id: RunId,
        /// Zero-based turn index within the run.
        turn: u32,
        /// Model selected for this request.
        model: ModelRef,
    },
    /// Reports context projection and every handoff loss before a model call.
    ContextPrepared {
        /// Zero-based turn index within the run.
        turn: u32,
        /// Target provider and model.
        target: ModelRef,
        /// Structured loss report for this projection.
        report: HandoffReport,
    },
    /// Begins the lifecycle of one canonical or custom record.
    MessageStarted {
        /// Stable lifecycle identity. Canonical messages use their persisted ID.
        message_id: MessageId,
        /// Message role.
        role: MessageRole,
    },
    /// Carries one lossless normalized assistant stream event.
    AssistantUpdate {
        /// Stable assistant message identity.
        message_id: MessageId,
        /// Provider-neutral assistant event.
        event: AssistantEvent,
    },
    /// Commits one complete durable transcript record.
    MessageCommitted {
        /// Record appended to durable agent state.
        message: AgentRecord,
    },
    /// Begins execution of one finalized tool call.
    ToolExecutionStarted {
        /// Source assistant tool call.
        call: ToolCall,
    },
    /// Carries one transient tool execution update.
    ToolExecutionUpdated {
        /// Stable executing call identity.
        call_id: ToolCallId,
        /// Scratch-free update value.
        update: ToolUpdate,
    },
    /// Finishes one tool execution after postprocessing.
    ToolExecutionFinished {
        /// Stable executing call identity.
        call_id: ToolCallId,
        /// Final tool output before transcript conversion.
        result: ToolOutput,
        /// Whether the finalized result represents a tool error.
        is_error: bool,
    },
    /// Finishes one assistant response and its complete tool batch.
    TurnFinished {
        /// Stable facts about committed turn records.
        outcome: TurnOutcome,
    },
    /// Finishes the run. No later event may use this run identity.
    RunFinished {
        /// Expected operational outcome.
        outcome: RunOutcome,
    },
}

impl<'de> Deserialize<'de> for AgentEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct EventTag {
            r#type: String,
        }
        #[derive(Deserialize)]
        struct RunStartedFields {
            run_id: RunId,
        }
        #[derive(Deserialize)]
        struct TurnStartedFields {
            run_id: RunId,
            turn: u32,
            model: ModelRef,
        }
        #[derive(Deserialize)]
        struct ContextPreparedFields {
            turn: u32,
            target: ModelRef,
            report: HandoffReport,
        }
        #[derive(Deserialize)]
        struct MessageStartedFields {
            message_id: MessageId,
            role: MessageRole,
        }
        #[derive(Deserialize)]
        struct AssistantUpdateFields {
            message_id: MessageId,
            event: AssistantEvent,
        }
        #[derive(Deserialize)]
        struct MessageCommittedFields {
            message: AgentRecord,
        }
        #[derive(Deserialize)]
        struct ToolExecutionStartedFields {
            call: ToolCall,
        }
        #[derive(Deserialize)]
        struct ToolExecutionUpdatedFields {
            call_id: ToolCallId,
            update: ToolUpdate,
        }
        #[derive(Deserialize)]
        struct ToolExecutionFinishedFields {
            call_id: ToolCallId,
            result: ToolOutput,
            is_error: bool,
        }
        #[derive(Deserialize)]
        struct TurnFinishedFields {
            outcome: TurnOutcome,
        }
        #[derive(Deserialize)]
        struct RunFinishedFields {
            outcome: RunOutcome,
        }

        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let tag = serde_json::from_str::<EventTag>(raw.get()).map_err(de::Error::custom)?;
        match tag.r#type.as_str() {
            "run_started" => {
                let fields = serde_json::from_str::<RunStartedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::RunStarted {
                    run_id: fields.run_id,
                })
            }
            "turn_started" => {
                let fields = serde_json::from_str::<TurnStartedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::TurnStarted {
                    run_id: fields.run_id,
                    turn: fields.turn,
                    model: fields.model,
                })
            }
            "context_prepared" => {
                let fields = serde_json::from_str::<ContextPreparedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::ContextPrepared {
                    turn: fields.turn,
                    target: fields.target,
                    report: fields.report,
                })
            }
            "message_started" => {
                let fields = serde_json::from_str::<MessageStartedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::MessageStarted {
                    message_id: fields.message_id,
                    role: fields.role,
                })
            }
            "assistant_update" => {
                let fields = serde_json::from_str::<AssistantUpdateFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::AssistantUpdate {
                    message_id: fields.message_id,
                    event: fields.event,
                })
            }
            "message_committed" => {
                let fields = serde_json::from_str::<MessageCommittedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::MessageCommitted {
                    message: fields.message,
                })
            }
            "tool_execution_started" => {
                let fields = serde_json::from_str::<ToolExecutionStartedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::ToolExecutionStarted { call: fields.call })
            }
            "tool_execution_updated" => {
                let fields = serde_json::from_str::<ToolExecutionUpdatedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::ToolExecutionUpdated {
                    call_id: fields.call_id,
                    update: fields.update,
                })
            }
            "tool_execution_finished" => {
                let fields = serde_json::from_str::<ToolExecutionFinishedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::ToolExecutionFinished {
                    call_id: fields.call_id,
                    result: fields.result,
                    is_error: fields.is_error,
                })
            }
            "turn_finished" => {
                let fields = serde_json::from_str::<TurnFinishedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::TurnFinished {
                    outcome: fields.outcome,
                })
            }
            "run_finished" => {
                let fields = serde_json::from_str::<RunFinishedFields>(raw.get())
                    .map_err(de::Error::custom)?;
                Ok(Self::RunFinished {
                    outcome: fields.outcome,
                })
            }
            other => Err(de::Error::unknown_variant(
                other,
                &[
                    "run_started",
                    "turn_started",
                    "context_prepared",
                    "message_started",
                    "assistant_update",
                    "message_committed",
                    "tool_execution_started",
                    "tool_execution_updated",
                    "tool_execution_finished",
                    "turn_finished",
                    "run_finished",
                ],
            )),
        }
    }
}

/// Monotonically sequenced event envelope used for persistence and FFI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentEventEnvelope {
    /// Global event sequence allocated by this agent instance.
    pub sequence: u64,
    /// Run to which this event belongs.
    pub run_id: RunId,
    /// Ordered state-machine event.
    pub event: AgentEvent,
}
