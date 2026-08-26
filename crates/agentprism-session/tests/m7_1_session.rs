use agentprism_ai::{
    ApiId, AssistantFinish, AssistantFinishReason, AssistantMessage, ContentBlock, ContentBlockId,
    Cost, Currency, Message, MessageId, ModelId, ProviderId, ReplayEnvelope, ReplayScope, RunId,
    Timestamp, ToolCall, ToolCallId, Usage, UsageSource,
};
use agentprism_core::AgentRecord;
use agentprism_session::*;
use futures_executor::block_on;
use serde_json::value::RawValue;
use static_assertions::assert_impl_all;
use std::{rc::Rc, sync::Arc};

fn timestamp(value: u64) -> Timestamp {
    Timestamp::from_unix_millis(i64::try_from(value).expect("test timestamp fits i64"))
}

fn custom_entry(
    sequence: u64,
    id: &str,
    parent_id: Option<&str>,
    lane: Option<&str>,
) -> SessionMutation {
    SessionMutation::Entry {
        lane: lane.map(LaneName::new),
        entry: SessionEntry::Custom {
            base: EntryBase {
                id: EntryId::new(id),
                sequence: Sequence::new(sequence),
                parent_id: parent_id.map(EntryId::new),
                timestamp: timestamp(sequence),
            },
            custom_type: "note".to_owned(),
            data: None,
        },
    }
}

fn message_entry(
    sequence: u64,
    id: &str,
    parent_id: Option<&str>,
    lane: Option<&str>,
) -> SessionMutation {
    SessionMutation::Entry {
        lane: lane.map(LaneName::new),
        entry: SessionEntry::Message {
            base: EntryBase {
                id: EntryId::new(id),
                sequence: Sequence::new(sequence),
                parent_id: parent_id.map(EntryId::new),
                timestamp: timestamp(sequence),
            },
            message: AgentRecord::Custom {
                type_name: "test_message".to_owned(),
                payload: RawValue::from_string("null".to_owned()).expect("valid raw JSON"),
            },
            terminate: false,
        },
    }
}

fn assistant_tool_entry(
    sequence: u64,
    id: &str,
    parent_id: Option<&str>,
    lane: Option<&str>,
    call_id: &str,
    tool_name: &str,
) -> SessionMutation {
    let provider = ProviderId::new("scripted");
    let api = ApiId::new("scripted");
    let model = ModelId::new("test-model");
    let assistant = AssistantMessage {
        id: MessageId::new(format!("message-{id}")),
        provider: provider.clone(),
        api: api.clone(),
        requested_model: model.clone(),
        response_model: None,
        response_id: None,
        deferred: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::ToolCall {
            id: ContentBlockId::new(format!("block-{id}")),
            call: ToolCall {
                id: ToolCallId::new(call_id),
                name: tool_name.to_owned(),
                arguments: serde_json::json!({}),
            },
        }],
        replay: ReplayEnvelope::new(ReplayScope::new(provider, api, model.clone(), model)),
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::ToolUse,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: timestamp(sequence),
    };
    SessionMutation::Entry {
        lane: lane.map(LaneName::new),
        entry: SessionEntry::Message {
            base: EntryBase {
                id: EntryId::new(id),
                sequence: Sequence::new(sequence),
                parent_id: parent_id.map(EntryId::new),
                timestamp: timestamp(sequence),
            },
            message: AgentRecord::Llm(Message::Assistant(assistant)),
            terminate: false,
        },
    }
}

fn record_base(sequence: u64, id: &str, lane: &str) -> OperationRecordBase {
    OperationRecordBase {
        id: OperationRecordId::new(id),
        sequence: Sequence::new(sequence),
        lane: LaneName::new(lane),
        timestamp: timestamp(sequence),
    }
}

fn run_intent(prompt_type: &str) -> OperationIntent {
    OperationIntent::Run {
        original_prompt: vec![AgentRecord::Custom {
            type_name: prompt_type.to_owned(),
            payload: RawValue::from_string("{}".to_owned()).expect("valid raw JSON"),
        }],
        initial_messages: Vec::new(),
        system_prompt_override: None,
        resume_data: Default::default(),
    }
}

fn operation_started(sequence: u64, id: &str, lane: &str) -> SessionMutation {
    SessionMutation::Record {
        record: OperationRecord::Started {
            base: record_base(sequence, id, lane),
            source_leaf_id: None,
            intent: run_intent("prompt"),
        },
    }
}

fn operation_finished(sequence: u64, id: &str, run_id: &str, lane: &str) -> SessionMutation {
    SessionMutation::Record {
        record: OperationRecord::Finished {
            base: record_base(sequence, id, lane),
            run_id: RunId::new(run_id),
            outcome: OperationOutcome::Completed,
            error: None,
        },
    }
}

fn queue_target(id: &str) -> ProvisionedEntry {
    ProvisionedEntry::Message {
        id: EntryId::new(id),
        message: AgentRecord::Custom {
            type_name: "test_message".to_owned(),
            payload: RawValue::from_string("null".to_owned()).expect("valid raw JSON"),
        },
        terminate: false,
    }
}

fn queue_enqueued(sequence: u64, id: &str, run_id: &str, target_id: &str) -> SessionMutation {
    queue_enqueued_on_lane(sequence, id, "main", run_id, target_id)
}

fn queue_enqueued_on_lane(
    sequence: u64,
    id: &str,
    lane: &str,
    run_id: &str,
    target_id: &str,
) -> SessionMutation {
    SessionMutation::Record {
        record: OperationRecord::QueueEnqueued {
            base: record_base(sequence, id, lane),
            run_id: Some(RunId::new(run_id)),
            queue: QueueKind::Steer,
            target: queue_target(target_id),
        },
    }
}

fn queue_cancelled(sequence: u64, id: &str, run_id: &str, entry_id: &str) -> SessionMutation {
    queue_cancelled_on_lane(sequence, id, "main", run_id, entry_id)
}

fn queue_cancelled_on_lane(
    sequence: u64,
    id: &str,
    lane: &str,
    run_id: &str,
    entry_id: &str,
) -> SessionMutation {
    SessionMutation::Record {
        record: OperationRecord::QueueCancelled {
            base: record_base(sequence, id, lane),
            run_id: Some(RunId::new(run_id)),
            entry_id: EntryId::new(entry_id),
        },
    }
}

fn tool_started(
    sequence: u64,
    id: &str,
    run_id: &str,
    assistant_entry_id: &str,
    tool_index: u32,
    call_id: &str,
    tool_name: &str,
) -> SessionMutation {
    tool_started_on_lane(
        sequence,
        id,
        "main",
        run_id,
        assistant_entry_id,
        tool_index,
        call_id,
        tool_name,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "test helper mirrors the complete durable tool-start identity"
)]
fn tool_started_on_lane(
    sequence: u64,
    id: &str,
    lane: &str,
    run_id: &str,
    assistant_entry_id: &str,
    tool_index: u32,
    call_id: &str,
    tool_name: &str,
) -> SessionMutation {
    SessionMutation::Record {
        record: OperationRecord::ToolStarted {
            base: record_base(sequence, id, lane),
            run_id: RunId::new(run_id),
            assistant_entry_id: EntryId::new(assistant_entry_id),
            tool_index,
            call: ToolCallIdentity {
                id: ToolCallId::new(call_id),
                name: tool_name.to_owned(),
            },
            effective_args: serde_json::json!({}),
            result_entry_id: EntryId::new(format!("result-{id}")),
            replay: ToolReplayPolicy::Never,
        },
    }
}

fn usage_record(sequence: u64, id: &str) -> SessionMutation {
    SessionMutation::Record {
        record: OperationRecord::Usage {
            base: record_base(sequence, id, "main"),
            attribution: UsageAttribution::Adjustment {
                run_id: None,
                entry_id: None,
                details: None,
            },
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: None,
                cache_read_tokens: Some(3),
                cache_write_tokens: Some(2),
                cache_write_one_hour_tokens: None,
                total_tokens: Some(20),
                source: UsageSource::ProviderReported,
            },
            cost: Some(Cost {
                currency: Currency::usd(),
                micros: 10,
            }),
            adjustment: None,
        },
    }
}

fn apply(state: &mut SessionState, mutation: SessionMutation) {
    state.apply(&mutation).expect("test mutation must reduce");
}

fn create_request(id: &str, created_at: i64) -> CreateSessionRequest {
    CreateSessionRequest::new(
        id,
        Timestamp::from_unix_millis(created_at),
        SessionEnvironmentMetadata::default(),
    )
}

#[test]
fn session_sequence_starts_at_one() {
    // Pi basis: packages/agent/src/harness/session/state.ts and memory conformance
    // "assigns parents and one sequence across every mutation".
    let mut state = SessionState::new();
    assert_eq!(state.next_sequence().unwrap(), Sequence::FIRST);
    apply(&mut state, custom_entry(1, "root", None, Some("main")));
    assert_eq!(state.sequence(), Sequence::FIRST);
}

#[test]
fn session_sequence_is_global_across_mutation_kinds() {
    // Pi basis: packages/agent/src/harness/session/state.ts applyMutation and
    // packages/agent/src/harness/session/testing/conformance.ts.
    let mut state = SessionState::new();
    apply(&mut state, custom_entry(1, "root", None, Some("main")));
    apply(
        &mut state,
        SessionMutation::Lane {
            sequence: Sequence::new(2),
            lane: LaneName::new("thread"),
            leaf_id: Some(EntryId::new("root")),
        },
    );
    apply(
        &mut state,
        SessionMutation::Fact {
            sequence: Sequence::new(3),
            fact: SessionFact::Name {
                name: Some("Example".to_owned()),
            },
        },
    );
    apply(&mut state, operation_started(4, "run", "thread"));
    assert_eq!(
        state
            .log()
            .iter()
            .map(SessionMutation::sequence)
            .collect::<Vec<_>>(),
        (1..=4).map(Sequence::new).collect::<Vec<_>>()
    );
}

#[test]
fn session_sequence_gap_is_corruption() {
    // Pi basis: packages/agent/src/harness/session/state.ts rejects every
    // non-consecutive imported mutation.
    let mut state = SessionState::new();
    let error = state
        .apply(&custom_entry(2, "gap", None, Some("main")))
        .unwrap_err();
    assert_eq!(
        error,
        SessionReductionError::SequenceGap {
            expected: Sequence::FIRST,
            actual: Sequence::new(2),
        }
    );
    assert_eq!(state.sequence(), Sequence::ZERO);
    assert!(state.log().is_empty());
}

#[test]
fn session_entry_parent_must_exist() {
    // Pi basis: packages/agent/src/harness/session/state.ts validates parentId
    // before publishing an entry mutation.
    let mut state = SessionState::new();
    let error = state
        .apply(&custom_entry(1, "orphan", Some("missing"), None))
        .unwrap_err();
    assert_eq!(
        error,
        SessionReductionError::MissingParent {
            parent_id: EntryId::new("missing"),
        }
    );
    assert!(state.entry(&EntryId::new("orphan")).is_none());
}

#[test]
fn session_lane_head_moves_on_append() {
    // Pi basis: packages/agent/src/harness/session/memory.ts appendEntry and
    // packages/agent/src/harness/session/state.ts lane-bound entry reduction.
    let mut state = SessionState::new();
    apply(&mut state, custom_entry(1, "root", None, Some("main")));
    apply(
        &mut state,
        custom_entry(2, "child", Some("root"), Some("main")),
    );
    assert_eq!(
        state.lane_leaf(&LaneName::new("main")),
        Some(&Some(EntryId::new("child")))
    );
}

#[test]
fn session_lane_can_move_to_ancestor() {
    // Pi basis: packages/agent/src/harness/session/memory.ts moveLane permits
    // any existing entry, including an ancestor.
    let mut state = SessionState::new();
    apply(&mut state, custom_entry(1, "root", None, Some("main")));
    apply(
        &mut state,
        custom_entry(2, "child", Some("root"), Some("main")),
    );
    apply(
        &mut state,
        SessionMutation::Lane {
            sequence: Sequence::new(3),
            lane: LaneName::new("main"),
            leaf_id: Some(EntryId::new("root")),
        },
    );
    assert_eq!(
        state.lane_leaf(&LaneName::new("main")),
        Some(&Some(EntryId::new("root")))
    );
}

#[test]
fn session_multiple_lanes_share_entry_tree() {
    // Pi basis: packages/agent/src/harness/session/testing/conformance.ts
    // "isolates lanes while sharing the tree".
    let mut state = SessionState::new();
    apply(&mut state, custom_entry(1, "root", None, Some("main")));
    apply(
        &mut state,
        SessionMutation::Lane {
            sequence: Sequence::new(2),
            lane: LaneName::new("thread"),
            leaf_id: Some(EntryId::new("root")),
        },
    );
    apply(
        &mut state,
        custom_entry(3, "main-child", Some("root"), Some("main")),
    );
    apply(
        &mut state,
        custom_entry(4, "thread-child", Some("root"), Some("thread")),
    );
    assert_eq!(
        state
            .scan_branch_root_to_leaf(&EntryId::new("main-child"))
            .unwrap()
            .iter()
            .map(|entry| entry.id().as_str())
            .collect::<Vec<_>>(),
        ["root", "main-child"]
    );
    assert_eq!(
        state
            .scan_branch_root_to_leaf(&EntryId::new("thread-child"))
            .unwrap()
            .iter()
            .map(|entry| entry.id().as_str())
            .collect::<Vec<_>>(),
        ["root", "thread-child"]
    );
}

#[test]
fn session_branch_scan_leaf_to_root() {
    // Pi basis: packages/agent/src/harness/session/state.ts walkToRoot and
    // findEntriesOnBranch default ordering.
    let mut state = SessionState::new();
    apply(&mut state, custom_entry(1, "root", None, Some("main")));
    apply(
        &mut state,
        custom_entry(2, "middle", Some("root"), Some("main")),
    );
    apply(
        &mut state,
        custom_entry(3, "leaf", Some("middle"), Some("main")),
    );
    assert_eq!(
        state
            .scan_branch_leaf_to_root(&EntryId::new("leaf"))
            .unwrap()
            .iter()
            .map(|entry| entry.id().as_str())
            .collect::<Vec<_>>(),
        ["leaf", "middle", "root"]
    );
}

#[test]
fn session_global_entry_query_sequence_order() {
    // Pi basis: packages/agent/src/harness/session/state.ts findEntries stores
    // every branch in one global entry sequence.
    let mut state = SessionState::new();
    apply(&mut state, custom_entry(1, "root", None, Some("main")));
    apply(
        &mut state,
        SessionMutation::Lane {
            sequence: Sequence::new(2),
            lane: LaneName::new("thread"),
            leaf_id: Some(EntryId::new("root")),
        },
    );
    apply(
        &mut state,
        custom_entry(3, "main-child", Some("root"), Some("main")),
    );
    apply(
        &mut state,
        custom_entry(4, "thread-child", Some("root"), Some("thread")),
    );
    assert_eq!(
        state
            .entries_in_sequence_order()
            .iter()
            .map(|entry| entry.id().as_str())
            .collect::<Vec<_>>(),
        ["root", "main-child", "thread-child"]
    );
}

#[test]
fn session_fact_latest_value_wins() {
    // Pi basis: packages/agent/src/harness/session/state.ts fact reduction and
    // memory conformance "keeps latest-value facts".
    let mut state = SessionState::new();
    for (sequence, name) in [(1, Some("First")), (2, Some("Second")), (3, None)] {
        apply(
            &mut state,
            SessionMutation::Fact {
                sequence: Sequence::new(sequence),
                fact: SessionFact::Name {
                    name: name.map(str::to_owned),
                },
            },
        );
    }
    assert_eq!(state.name(), None);
    assert_eq!(state.log().len(), 3);
}

#[test]
fn session_label_is_global_not_branch_scoped() {
    // Pi basis: packages/agent/src/harness/session/types.ts declares labels as
    // global facts and memory conformance verifies latest-value lookup.
    let mut state = SessionState::new();
    apply(&mut state, custom_entry(1, "root", None, Some("main")));
    apply(
        &mut state,
        SessionMutation::Lane {
            sequence: Sequence::new(2),
            lane: LaneName::new("thread"),
            leaf_id: Some(EntryId::new("root")),
        },
    );
    apply(
        &mut state,
        SessionMutation::Fact {
            sequence: Sequence::new(3),
            fact: SessionFact::Label {
                target_id: EntryId::new("root"),
                label: Some("checkpoint".to_owned()),
            },
        },
    );
    assert_eq!(state.label(&EntryId::new("root")), Some("checkpoint"));
    assert_eq!(state.lanes().len(), 2);
}

#[test]
fn session_stats_derive_from_usage_records() {
    // Pi basis: packages/agent/src/harness/session/state.ts increments ledger
    // statistics only for usage operation records.
    let mut state = SessionState::new();
    apply(&mut state, message_entry(1, "message", None, Some("main")));
    apply(&mut state, usage_record(2, "usage"));
    apply(
        &mut state,
        SessionMutation::Record {
            record: OperationRecord::Usage {
                base: record_base(3, "adjustment", "main"),
                attribution: UsageAttribution::Adjustment {
                    run_id: None,
                    entry_id: None,
                    details: None,
                },
                usage: Usage::zero(UsageSource::Unknown),
                cost: None,
                adjustment: Some(SignedUsageAdjustment {
                    input_tokens: -2,
                    total_tokens: -2,
                    cost: Some(Cost {
                        currency: Currency::usd(),
                        micros: -1,
                    }),
                    ..SignedUsageAdjustment::default()
                }),
            },
        },
    );
    apply(
        &mut state,
        SessionMutation::Record {
            record: OperationRecord::Usage {
                base: record_base(4, "explicit-zero-total", "main"),
                attribution: UsageAttribution::Adjustment {
                    run_id: None,
                    entry_id: None,
                    details: None,
                },
                usage: Usage {
                    input_tokens: 7,
                    output_tokens: 5,
                    reasoning_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    cache_write_one_hour_tokens: None,
                    total_tokens: Some(0),
                    source: UsageSource::ProviderReported,
                },
                cost: None,
                adjustment: None,
            },
        },
    );
    apply(
        &mut state,
        SessionMutation::Record {
            record: OperationRecord::Usage {
                base: record_base(5, "derived-total", "main"),
                attribution: UsageAttribution::Adjustment {
                    run_id: None,
                    entry_id: None,
                    details: None,
                },
                usage: Usage {
                    input_tokens: 4,
                    output_tokens: 3,
                    reasoning_tokens: Some(1),
                    cache_read_tokens: Some(2),
                    cache_write_tokens: Some(1),
                    cache_write_one_hour_tokens: None,
                    total_tokens: None,
                    source: UsageSource::ProviderReported,
                },
                cost: None,
                adjustment: None,
            },
        },
    );
    assert_eq!(state.stats().message_count, 1);
    assert_eq!(state.stats().cached_tokens, 5);
    assert_eq!(state.stats().uncached_tokens, 22);
    // Pi's session ledger adds usage.totalTokens directly. Context planning's
    // truthy-zero fallback must not turn the explicit zero into 12 tokens;
    // absent totals are derived from normalized components, excluding the
    // reasoning subset to avoid double-counting it with output tokens.
    assert_eq!(state.stats().total_tokens, 28);
    assert_eq!(state.stats().cost_micros_by_currency.get("USD"), Some(&9));
}

#[test]
fn session_tool_invocation_identity_is_assistant_entry_and_index() {
    // Pi basis: packages/agent/src/harness/reducer.ts validateToolStart keys
    // invocations by assistantEntryId and toolIndex, independent of runId.
    let mut state = SessionState::new();
    apply(
        &mut state,
        assistant_tool_entry(1, "assistant", None, Some("main"), "call-1", "tool-1"),
    );
    apply(&mut state, operation_started(2, "run-1", "main"));
    apply(
        &mut state,
        tool_started(
            3,
            "tool-start-1",
            "run-1",
            "assistant",
            0,
            "call-1",
            "tool-1",
        ),
    );
    apply(
        &mut state,
        operation_finished(4, "finish-1", "run-1", "main"),
    );
    apply(&mut state, operation_started(5, "run-2", "main"));

    let error = state
        .apply(&tool_started(
            6,
            "tool-start-2",
            "run-2",
            "assistant",
            0,
            "call-1",
            "tool-1",
        ))
        .unwrap_err();
    assert_eq!(
        error,
        SessionReductionError::DuplicateToolInvocation {
            assistant_entry_id: EntryId::new("assistant"),
            tool_index: 0,
        }
    );
    assert_eq!(state.sequence(), Sequence::new(5));
}

#[test]
fn session_tool_invocation_identity_is_lane_scoped() {
    // Pi basis: packages/agent/src/harness/reducer.ts validateRecordLog keeps
    // its duplicate-invocation set inside one lane's RecordLogSlice. Two lanes
    // may therefore recover the same immutable assistant entry independently.
    let mut state = SessionState::new();
    apply(
        &mut state,
        assistant_tool_entry(1, "assistant", None, Some("main"), "call-1", "tool-1"),
    );
    apply(
        &mut state,
        SessionMutation::Lane {
            sequence: Sequence::new(2),
            lane: LaneName::new("thread"),
            leaf_id: Some(EntryId::new("assistant")),
        },
    );
    apply(&mut state, operation_started(3, "main-run", "main"));
    apply(
        &mut state,
        tool_started_on_lane(
            4,
            "main-tool-start",
            "main",
            "main-run",
            "assistant",
            0,
            "call-1",
            "tool-1",
        ),
    );
    apply(&mut state, operation_started(5, "thread-run", "thread"));
    apply(
        &mut state,
        tool_started_on_lane(
            6,
            "thread-tool-start",
            "thread",
            "thread-run",
            "assistant",
            0,
            "call-1",
            "tool-1",
        ),
    );

    assert_eq!(state.sequence(), Sequence::new(6));
    assert_eq!(state.records_in_sequence_order().len(), 4);
}

#[test]
fn session_tool_start_rejects_provisioned_but_uncommitted_assistant() {
    // Pi basis: packages/agent/src/harness/reducer.ts validateToolStart
    // requires an existing assistant entry; a step result id is not enough.
    let mut state = SessionState::new();
    apply(&mut state, operation_started(1, "run", "main"));
    apply(
        &mut state,
        SessionMutation::Record {
            record: OperationRecord::StepAttempt {
                base: record_base(2, "attempt", "main"),
                run_id: RunId::new("run"),
                step: OperationStep::Assistant,
                attempt: 1,
                result_entry_id: EntryId::new("assistant-pending"),
                compaction_reason: None,
            },
        },
    );

    let error = state
        .apply(&tool_started(
            3,
            "tool-start",
            "run",
            "assistant-pending",
            0,
            "arbitrary-call",
            "arbitrary-tool",
        ))
        .unwrap_err();
    assert_eq!(
        error,
        SessionReductionError::MissingToolAssistant {
            assistant_entry_id: EntryId::new("assistant-pending"),
        }
    );
    assert_eq!(state.sequence(), Sequence::new(2));
}

#[test]
fn session_tool_start_must_match_committed_assistant_call() {
    // Pi basis: packages/agent/src/harness/reducer.ts validateToolStart checks
    // the filtered assistant tool-call ordinal, id, and name.
    let mut state = SessionState::new();
    apply(
        &mut state,
        assistant_tool_entry(1, "assistant", None, Some("main"), "call-1", "tool-1"),
    );
    apply(&mut state, operation_started(2, "run", "main"));

    let error = state
        .apply(&tool_started(
            3,
            "tool-start",
            "run",
            "assistant",
            0,
            "different-call",
            "tool-1",
        ))
        .unwrap_err();
    assert_eq!(
        error,
        SessionReductionError::ToolIdentityMismatch {
            assistant_entry_id: EntryId::new("assistant"),
            tool_index: 0,
        }
    );
    assert_eq!(state.sequence(), Sequence::new(2));
}

#[test]
fn session_queue_cancellation_requires_pending_matching_enqueue() {
    // Pi basis: packages/agent/src/harness/reducer.ts validateRecordLog requires
    // an earlier enqueue whose runId exactly matches the cancellation and whose
    // provisioned target has not been committed.
    let mut committed = SessionState::new();
    apply(&mut committed, operation_started(1, "run-1", "main"));
    apply(
        &mut committed,
        queue_enqueued(2, "enqueue", "run-1", "queued-message"),
    );
    apply(
        &mut committed,
        message_entry(3, "queued-message", None, Some("main")),
    );
    assert_eq!(
        committed
            .apply(&queue_cancelled(
                4,
                "cancel-after-commit",
                "run-1",
                "queued-message",
            ))
            .unwrap_err(),
        SessionReductionError::MissingQueuedEntry {
            entry_id: EntryId::new("queued-message"),
        }
    );
    assert_eq!(committed.sequence(), Sequence::new(3));

    let mut mismatched_run = SessionState::new();
    apply(&mut mismatched_run, operation_started(1, "run-1", "main"));
    apply(
        &mut mismatched_run,
        queue_enqueued(2, "enqueue", "run-1", "queued-message"),
    );
    assert_eq!(
        mismatched_run
            .apply(&queue_cancelled(
                3,
                "cancel-wrong-run",
                "run-2",
                "queued-message",
            ))
            .unwrap_err(),
        SessionReductionError::MissingQueuedEntry {
            entry_id: EntryId::new("queued-message"),
        }
    );
    assert_eq!(mismatched_run.sequence(), Sequence::new(2));

    apply(
        &mut mismatched_run,
        queue_cancelled(3, "cancel-correct-run", "run-1", "queued-message"),
    );
}

#[test]
fn session_queue_cancellation_is_idempotent_during_reduction() {
    // Pi basis: packages/agent/src/harness/reducer.ts:360-381 retains enqueue
    // history while deriving effective cancellation separately, so repeated
    // cancellations of the same still-uncommitted target remain valid.
    let mut state = SessionState::new();
    apply(&mut state, operation_started(1, "run-1", "main"));
    apply(
        &mut state,
        queue_enqueued(2, "enqueue", "run-1", "queued-message"),
    );
    apply(
        &mut state,
        queue_cancelled(3, "cancel-first", "run-1", "queued-message"),
    );
    apply(
        &mut state,
        queue_cancelled(4, "cancel-second", "run-1", "queued-message"),
    );

    assert_eq!(state.sequence(), Sequence::new(4));
    assert_eq!(state.records_in_sequence_order().len(), 4);
}

#[test]
fn session_queue_cancellation_is_lane_scoped() {
    // Pi basis: packages/agent/src/harness/reducer.ts validates one
    // RecordLogSlice at a time, so queue identities are lane-local even when
    // provisioned entry ids and run ids collide across lanes.
    let mut cross_lane_authorization = SessionState::new();
    apply(
        &mut cross_lane_authorization,
        SessionMutation::Lane {
            sequence: Sequence::FIRST,
            lane: LaneName::new("thread"),
            leaf_id: None,
        },
    );
    apply(
        &mut cross_lane_authorization,
        queue_enqueued_on_lane(2, "main-enqueue", "main", "run", "shared-target"),
    );
    assert_eq!(
        cross_lane_authorization
            .apply(&queue_cancelled_on_lane(
                3,
                "thread-cancel",
                "thread",
                "run",
                "shared-target",
            ))
            .unwrap_err(),
        SessionReductionError::MissingQueuedEntry {
            entry_id: EntryId::new("shared-target"),
        }
    );
    apply(
        &mut cross_lane_authorization,
        queue_cancelled_on_lane(3, "main-cancel", "main", "run", "shared-target"),
    );

    let mut cross_lane_overwrite = SessionState::new();
    apply(
        &mut cross_lane_overwrite,
        SessionMutation::Lane {
            sequence: Sequence::FIRST,
            lane: LaneName::new("thread"),
            leaf_id: None,
        },
    );
    apply(
        &mut cross_lane_overwrite,
        queue_enqueued_on_lane(2, "main-enqueue", "main", "main-run", "shared-target"),
    );
    apply(
        &mut cross_lane_overwrite,
        queue_enqueued_on_lane(3, "thread-enqueue", "thread", "thread-run", "shared-target"),
    );
    apply(
        &mut cross_lane_overwrite,
        queue_cancelled_on_lane(4, "main-cancel", "main", "main-run", "shared-target"),
    );
    apply(
        &mut cross_lane_overwrite,
        queue_cancelled_on_lane(5, "thread-cancel", "thread", "thread-run", "shared-target"),
    );
}

#[test]
fn session_open_operation_detected() {
    // Pi basis: packages/agent/src/harness/session/state.ts
    // findOpenOperations and memory conformance recovery cases.
    let mut state = SessionState::new();
    apply(&mut state, operation_started(1, "run", "main"));
    let open = state.open_operations(&LaneName::new("main"));
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].run_id(), Some(RunId::new("run")));
    assert!(matches!(
        state.recovery_decision(&LaneName::new("main")),
        RecoveryDecision::Resume { .. }
    ));
}

#[test]
fn session_multiple_open_operations_is_corruption() {
    // Pi basis: packages/agent/src/harness/session/types.ts recovery contract
    // reads two open starts to distinguish corruption from suspension.
    let mut state = SessionState::new();
    apply(&mut state, operation_started(1, "first", "main"));
    apply(&mut state, operation_started(2, "second", "main"));
    let RecoveryDecision::Corrupt { open_operations } =
        state.recovery_decision(&LaneName::new("main"))
    else {
        panic!("two unresolved starts must be classified as corruption");
    };
    assert_eq!(
        open_operations
            .iter()
            .map(|record| record.run_id().unwrap().into_inner())
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
}

#[test]
fn session_operation_recovery_reconstructs_intent() {
    // Pi basis: packages/agent/src/harness/session/types.ts persists normalized
    // intent and step attempts specifically for suspended-operation recovery.
    let mut state = SessionState::new();
    apply(&mut state, operation_started(1, "run", "main"));
    apply(
        &mut state,
        SessionMutation::Record {
            record: OperationRecord::StepAttempt {
                base: record_base(2, "attempt", "main"),
                run_id: RunId::new("run"),
                step: OperationStep::Assistant,
                attempt: 1,
                result_entry_id: EntryId::new("assistant-result"),
                compaction_reason: None,
            },
        },
    );
    let RecoveryDecision::Resume {
        operation,
        completed_steps,
    } = state.recovery_decision(&LaneName::new("main"))
    else {
        panic!("one unresolved operation must be resumable");
    };
    let OperationRecord::Started { intent, .. } = operation else {
        panic!("recovery operation must be the start record");
    };
    assert_eq!(intent, run_intent("prompt"));
    assert_eq!(completed_steps.len(), 1);
    assert!(matches!(
        completed_steps[0],
        OperationRecord::StepAttempt {
            step: OperationStep::Assistant,
            attempt: 1,
            ..
        }
    ));
}

#[test]
fn session_operation_recovery_is_lane_scoped() {
    // Pi basis: packages/agent/src/harness/session/types.ts RecordQuery.lane
    // is an exact match, and packages/agent/src/harness/reducer.ts reconstructs
    // recovery from one lane's RecordLogSlice.
    let mut state = SessionState::new();
    apply(
        &mut state,
        SessionMutation::Lane {
            sequence: Sequence::FIRST,
            lane: LaneName::new("thread"),
            leaf_id: None,
        },
    );
    apply(&mut state, operation_started(2, "run", "main"));
    apply(
        &mut state,
        SessionMutation::Record {
            record: OperationRecord::StepAttempt {
                base: record_base(3, "thread-attempt", "thread"),
                run_id: RunId::new("run"),
                step: OperationStep::Assistant,
                attempt: 1,
                result_entry_id: EntryId::new("thread-result"),
                compaction_reason: None,
            },
        },
    );
    apply(
        &mut state,
        SessionMutation::Record {
            record: OperationRecord::StepAttempt {
                base: record_base(4, "main-attempt", "main"),
                run_id: RunId::new("run"),
                step: OperationStep::Assistant,
                attempt: 1,
                result_entry_id: EntryId::new("main-result"),
                compaction_reason: None,
            },
        },
    );
    apply(
        &mut state,
        SessionMutation::Record {
            record: OperationRecord::AbortRequested {
                base: record_base(5, "thread-abort", "thread"),
                run_id: RunId::new("run"),
            },
        },
    );

    let RecoveryDecision::Resume {
        completed_steps, ..
    } = state.recovery_decision(&LaneName::new("main"))
    else {
        panic!("another lane's abort request must not abandon the main operation");
    };
    assert_eq!(completed_steps.len(), 1);
    assert_eq!(completed_steps[0].base().id.as_str(), "main-attempt");
    assert_eq!(completed_steps[0].lane(), &LaneName::new("main"));
}

#[test]
fn session_reducer_replay_equals_live_state() {
    // Pi basis: packages/agent/src/harness/session/state.ts is used both for
    // live mutation reduction and complete JSONL replay.
    let header = SessionHeader::new(
        "session",
        Timestamp::from_unix_millis(1),
        SessionEnvironmentMetadata::default(),
    );
    let storage = InMemorySessionStorage::new(header).unwrap();
    let mutations = vec![
        message_entry(1, "message", None, Some("main")),
        SessionMutation::Fact {
            sequence: Sequence::new(2),
            fact: SessionFact::Name {
                name: Some("Example".to_owned()),
            },
        },
        usage_record(3, "usage"),
    ];
    storage
        .append_batch(Sequence::ZERO, mutations.clone())
        .unwrap();
    let replayed = SessionState::replay(mutations).unwrap();
    assert_eq!(storage.state_snapshot().unwrap(), replayed);
}

#[test]
fn session_repository_branch_fork_copies_prefix_without_records() {
    // Pi basis: packages/agent/src/harness/session/state.ts createForkMutations
    // and memory conformance branch-fork cases.
    let repository = InMemorySessionRepository::new();
    let source = block_on(SessionRepository::create(
        &repository,
        create_request("source", 1),
    ))
    .unwrap();
    let mutations = vec![
        message_entry(1, "root", None, Some("main")),
        message_entry(2, "tail", Some("root"), Some("main")),
        operation_started(3, "run", "main"),
    ];
    block_on(source.append(Sequence::ZERO, mutations)).unwrap();

    let fork = block_on(SessionRepository::fork(
        &repository,
        &SessionId::new("source"),
        ForkRequest {
            session_id: SessionId::new("fork"),
            created_at: Timestamp::from_unix_millis(2),
            environment: SessionEnvironmentMetadata::default(),
            position: ForkPosition::Before(EntryId::new("tail")),
        },
    ))
    .unwrap();
    let state = block_on(fork.load_state()).unwrap();
    assert_eq!(
        state
            .entries_in_sequence_order()
            .iter()
            .map(|entry| entry.id().as_str())
            .collect::<Vec<_>>(),
        ["root"]
    );
    assert!(state.records_in_sequence_order().is_empty());
    assert_eq!(
        state.lane_leaf(&LaneName::new("main")),
        Some(&Some(EntryId::new("root")))
    );
}

#[test]
fn session_repository_tree_fork_preserves_lanes_and_facts() {
    // Pi basis: packages/agent/src/harness/session/state.ts tree fork copies
    // immutable entries, lane heads, name, and labels but not records.
    let repository = InMemorySessionRepository::new();
    let source = block_on(SessionRepository::create(
        &repository,
        create_request("source", 1),
    ))
    .unwrap();
    let mutations = vec![
        message_entry(1, "root", None, Some("main")),
        SessionMutation::Lane {
            sequence: Sequence::new(2),
            lane: LaneName::new("thread"),
            leaf_id: Some(EntryId::new("root")),
        },
        message_entry(3, "main-child", Some("root"), Some("main")),
        message_entry(4, "thread-child", Some("root"), Some("thread")),
        SessionMutation::Fact {
            sequence: Sequence::new(5),
            fact: SessionFact::Name {
                name: Some("Source".to_owned()),
            },
        },
        SessionMutation::Fact {
            sequence: Sequence::new(6),
            fact: SessionFact::Label {
                target_id: EntryId::new("thread-child"),
                label: Some("tip".to_owned()),
            },
        },
    ];
    block_on(source.append(Sequence::ZERO, mutations)).unwrap();
    let fork = block_on(SessionRepository::fork(
        &repository,
        &SessionId::new("source"),
        ForkRequest {
            session_id: SessionId::new("tree-fork"),
            created_at: Timestamp::from_unix_millis(2),
            environment: SessionEnvironmentMetadata::default(),
            position: ForkPosition::WholeTree,
        },
    ))
    .unwrap();
    let state = block_on(fork.load_state()).unwrap();
    assert_eq!(state.name(), Some("Source"));
    assert_eq!(state.label(&EntryId::new("thread-child")), Some("tip"));
    assert_eq!(
        state.lane_leaf(&LaneName::new("thread")),
        Some(&Some(EntryId::new("thread-child")))
    );
}

#[test]
fn session_tree_fork_preserves_pi_source_order() {
    // Pi basis: packages/agent/src/harness/session/state.ts createForkMutations
    // preserves Map insertion order for lanes and emits labels while walking
    // copiedEntries in source sequence order (lines 263-297).
    let repository = InMemorySessionRepository::new();
    let source = block_on(SessionRepository::create(
        &repository,
        create_request("source-order", 1),
    ))
    .unwrap();
    let mutations = vec![
        message_entry(1, "root", None, Some("main")),
        SessionMutation::Lane {
            sequence: Sequence::new(2),
            lane: LaneName::new("zeta"),
            leaf_id: Some(EntryId::new("root")),
        },
        SessionMutation::Lane {
            sequence: Sequence::new(3),
            lane: LaneName::new("alpha"),
            leaf_id: Some(EntryId::new("root")),
        },
        message_entry(4, "z-entry", Some("root"), Some("main")),
        message_entry(5, "a-entry", Some("root"), Some("zeta")),
        SessionMutation::Fact {
            sequence: Sequence::new(6),
            fact: SessionFact::Label {
                target_id: EntryId::new("z-entry"),
                label: Some("first-label".to_owned()),
            },
        },
        SessionMutation::Fact {
            sequence: Sequence::new(7),
            fact: SessionFact::Label {
                target_id: EntryId::new("a-entry"),
                label: Some("second-label".to_owned()),
            },
        },
    ];
    block_on(source.append(Sequence::ZERO, mutations)).unwrap();

    let fork = block_on(SessionRepository::fork(
        &repository,
        &SessionId::new("source-order"),
        ForkRequest {
            session_id: SessionId::new("ordered-fork"),
            created_at: Timestamp::from_unix_millis(2),
            environment: SessionEnvironmentMetadata::default(),
            position: ForkPosition::WholeTree,
        },
    ))
    .unwrap();
    let state = block_on(fork.load_state()).unwrap();

    assert_eq!(
        state
            .lanes()
            .iter()
            .map(|lane| lane.name.as_str())
            .collect::<Vec<_>>(),
        ["main", "zeta", "alpha"]
    );
    assert_eq!(
        state
            .log()
            .iter()
            .map(|mutation| match mutation {
                SessionMutation::Entry { entry, .. } => format!("entry:{}", entry.id()),
                SessionMutation::Lane { lane, .. } => format!("lane:{lane}"),
                SessionMutation::Fact {
                    fact: SessionFact::Label { target_id, .. },
                    ..
                } => format!("label:{target_id}"),
                SessionMutation::Fact {
                    fact: SessionFact::Name { .. },
                    ..
                } => "name".to_owned(),
                SessionMutation::Record { .. } => "record".to_owned(),
            })
            .collect::<Vec<_>>(),
        [
            "entry:root",
            "entry:z-entry",
            "entry:a-entry",
            "lane:main",
            "lane:zeta",
            "lane:alpha",
            "label:z-entry",
            "label:a-entry",
        ]
    );
    assert_eq!(
        state
            .log()
            .iter()
            .map(SessionMutation::sequence)
            .collect::<Vec<_>>(),
        (1..=8).map(Sequence::new).collect::<Vec<_>>()
    );
}

#[test]
fn session_storage_append_is_atomic_on_reduction_failure() {
    // Pi basis: packages/agent/test/harness/session/jsonl-storage.test.ts
    // "does not advance state or poison the write queue after an append failure".
    let storage = InMemorySessionStorage::new(SessionHeader::new(
        "session",
        Timestamp::from_unix_millis(1),
        SessionEnvironmentMetadata::default(),
    ))
    .unwrap();
    let error = storage
        .append_batch(
            Sequence::ZERO,
            vec![
                custom_entry(1, "root", None, Some("main")),
                custom_entry(3, "gap", Some("root"), Some("main")),
            ],
        )
        .unwrap_err();
    assert_eq!(error.kind, SessionErrorKind::Corruption);
    assert_eq!(storage.state_snapshot().unwrap().sequence(), Sequence::ZERO);
    storage
        .append_batch(
            Sequence::ZERO,
            vec![custom_entry(1, "valid", None, Some("main"))],
        )
        .unwrap();
}

#[test]
fn session_send_and_local_storage_trait_families_are_object_safe() {
    // Architecture basis: part 2 §7.3 and §9.2 require independent Send and
    // Local object-safe async families; the in-memory backend supports both.
    assert_impl_all!(InMemorySessionStorage: Send, Sync, SessionStorage, LocalSessionStorage);
    let storage = Rc::new(
        InMemorySessionStorage::new(SessionHeader::new(
            "local",
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .unwrap(),
    );
    let local: Rc<dyn LocalSessionStorage> = storage;
    assert_eq!(
        block_on(local.metadata()).unwrap().last_sequence,
        Sequence::ZERO
    );

    let storage = Arc::new(
        InMemorySessionStorage::new(SessionHeader::new(
            "send",
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .unwrap(),
    );
    let send: Arc<dyn SessionStorage> = storage;
    assert_eq!(
        block_on(send.metadata()).unwrap().last_sequence,
        Sequence::ZERO
    );
}
