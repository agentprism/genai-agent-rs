use pi_agent_core::*;
use pi_ai::*;
use proptest::prelude::*;
use serde_json::{json, value::RawValue};
use std::{rc::Rc, sync::Arc};

fn usage(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens,
        reasoning_tokens: Some(output_tokens / 2),
        cache_read_tokens: Some(3),
        cache_write_tokens: Some(1),
        cache_write_one_hour_tokens: None,
        source: UsageSource::ProviderReported,
    }
}

fn public_error() -> PublicError {
    PublicError {
        code: "provider_error".into(),
        message: "provider failed".into(),
        retryable: true,
        provider_code: Some("overloaded".into()),
        status: Some(503),
        request_id: Some("request-1".into()),
    }
}

fn cancellation_error() -> PublicError {
    PublicError {
        code: "cancelled".into(),
        message: "request cancelled".into(),
        retryable: false,
        provider_code: None,
        status: None,
        request_id: Some("request-2".into()),
    }
}

fn assistant_message(
    id: impl Into<MessageId>,
    text: impl Into<String>,
    reason: AssistantFinishReason,
    message_usage: Usage,
) -> AssistantMessage {
    let error = match reason {
        AssistantFinishReason::Error => Some(public_error()),
        AssistantFinishReason::Aborted => Some(cancellation_error()),
        AssistantFinishReason::Stop
        | AssistantFinishReason::Length
        | AssistantFinishReason::ToolUse
        | AssistantFinishReason::Deferred => None,
    };
    AssistantMessage {
        id: id.into(),
        provider: ProviderId::new("openai"),
        api: ApiId::new("openai-responses"),
        requested_model: ModelId::new("gpt-test"),
        response_model: Some(ModelId::new("gpt-test-2026-08-01")),
        response_id: Some("response-1".into()),
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("text-0"),
            text: text.into(),
        }],
        replay: ReplayEnvelope::new(ReplayScope::new(
            "openai",
            "openai-responses",
            "gpt-test",
            "gpt-test-2026-08-01",
        )),
        usage: message_usage,
        cost: None,
        finish: AssistantFinish {
            reason,
            raw_provider_reason: None,
            error,
        },
        timestamp: Timestamp::from_unix_millis(1_777_000_000_000),
    }
}

fn user_record(index: usize, text: impl Into<String>) -> AgentRecord {
    AgentRecord::Llm(Message::User(UserMessage {
        id: MessageId::new(format!("user-{index}")),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("user-block-{index}")),
            text: text.into(),
        }],
        timestamp: Timestamp::from_unix_millis(index as i64),
    }))
}

fn custom_record(type_name: &str, value: serde_json::Value) -> AgentRecord {
    AgentRecord::Custom {
        type_name: type_name.into(),
        payload: RawValue::from_string(serde_json::to_string(&value).unwrap()).unwrap(),
    }
}

fn base_state() -> AgentState {
    AgentState::new(
        "You are helpful.",
        ModelRef::new("openai", "gpt-test"),
        ReasoningLevel::Off,
    )
}

fn partial_snapshot() -> AssistantMessageSnapshot {
    let mut assembler = AssistantAssembler::new();
    assembler
        .apply(&AssistantEvent::MessageStarted {
            message_id: MessageId::new("assistant-streaming"),
            provider: ProviderId::new("openai"),
            api: ApiId::new("openai-responses"),
            model: ModelId::new("gpt-test"),
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::ContentBlockStarted {
            block_id: ContentBlockId::new("stream-text-0"),
            content_index: 0,
            kind: ContentBlockKind::Text,
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::TextDelta {
            block_id: ContentBlockId::new("stream-text-0"),
            delta: "partial".into(),
        })
        .unwrap();
    assembler.snapshot()
}

#[test]
fn agent_snapshot_round_trips() {
    // Architecture v2 part 1 §4.9 and part 2 §8.1. Pi basis:
    // packages/agent/src/agent.ts processEvents and public streaming state.
    let mut state = base_state();
    state.transcript = vec![
        user_record(0, "hello"),
        custom_record("notification", json!({"text": "working"})),
    ];
    let snapshot = AgentSnapshot {
        schema_version: AGENT_SNAPSHOT_SCHEMA_VERSION,
        state,
        next_sequence: 42,
        streaming: Some(partial_snapshot()),
        pending_tool_calls: Arc::from([ToolCallId::new("call-1"), ToolCallId::new("call-2")]),
    };

    let encoded = serde_json::to_vec(&snapshot).unwrap();
    let restored: AgentSnapshot = serde_json::from_slice(&encoded).unwrap();

    assert_eq!(restored, snapshot);
    let serialized = String::from_utf8(encoded).unwrap();
    assert!(!serialized.contains("arguments_scratch"));
    assert!(!serialized.contains("binary_chunks"));
}

proptest! {
    #[test]
    fn agent_committed_event_replay_reproduces_final_state(
        records in prop::collection::vec((any::<bool>(), "[a-z0-9]{0,24}"), 0..64)
    ) {
        // Architecture v2 part 1 §8 property: replaying committed transcript
        // events equals final AgentState. Pi basis: packages/agent/src/agent.ts
        // reduces message_end into state before listeners run.
        let initial = base_state();
        let run_id = RunId::new("run-property");
        let committed = records
            .into_iter()
            .enumerate()
            .map(|(index, (is_custom, value))| {
                if is_custom {
                    custom_record("property", json!({"index": index, "value": value}))
                } else {
                    user_record(index, value)
                }
            })
            .collect::<Vec<_>>();
        let events = committed
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, message)| AgentEventEnvelope {
                sequence: u64::try_from(index).unwrap() + AGENT_INITIAL_SEQUENCE,
                run_id: run_id.clone(),
                event: AgentEvent::MessageCommitted { message },
            })
            .collect::<Vec<_>>();
        let mut expected = initial.clone();
        expected.transcript = committed;

        let replayed = replay_committed_events(initial, &events).unwrap();
        prop_assert_eq!(replayed, expected);
    }
}

#[test]
fn agent_failed_assistant_commit_replays() {
    // §10.9 failure conformance. Pi basis: packages/agent/src/agent-loop.ts
    // streamAssistantResponse replaces the partial and emits message_end before
    // turn_end/agent_end for an error terminal.
    let failed = assistant_message(
        "assistant-failed",
        "partial response",
        AssistantFinishReason::Error,
        usage(7, 4),
    );
    let event = AgentEventEnvelope {
        sequence: AGENT_INITIAL_SEQUENCE,
        run_id: RunId::new("run-failed"),
        event: AgentEvent::MessageCommitted {
            message: AgentRecord::Llm(Message::Assistant(failed.clone())),
        },
    };

    let replayed = replay_committed_events(base_state(), [&event]).unwrap();
    assert_eq!(
        replayed.transcript.last(),
        Some(&AgentRecord::Llm(Message::Assistant(failed)))
    );
}

#[test]
fn agent_cancelled_assistant_commit_replays() {
    // §10.9 failure conformance. Pi basis: packages/agent/src/agent-loop.ts
    // handles aborted terminals through the same message commitment path.
    let cancelled = assistant_message(
        "assistant-cancelled",
        "partial response",
        AssistantFinishReason::Aborted,
        usage(5, 2),
    );
    let event = AgentEventEnvelope {
        sequence: AGENT_INITIAL_SEQUENCE,
        run_id: RunId::new("run-cancelled"),
        event: AgentEvent::MessageCommitted {
            message: AgentRecord::Llm(Message::Assistant(cancelled.clone())),
        },
    };

    let replayed = replay_committed_events(base_state(), [&event]).unwrap();
    assert_eq!(
        replayed.transcript.last(),
        Some(&AgentRecord::Llm(Message::Assistant(cancelled)))
    );
}

#[test]
fn agent_failed_partial_content_round_trips() {
    // §10.9 failure conformance. Pi basis: packages/agent/src/agent-loop.ts
    // retains the partial assistant object when the stream terminates in error.
    let failed = assistant_message(
        "assistant-partial",
        "content before failure",
        AssistantFinishReason::Error,
        usage(2, 3),
    );
    let bytes = serde_json::to_vec(&AgentRecord::Llm(Message::Assistant(failed))).unwrap();
    let restored: AgentRecord = serde_json::from_slice(&bytes).unwrap();
    let AgentRecord::Llm(Message::Assistant(restored)) = restored else {
        panic!("expected assistant record");
    };
    assert_eq!(
        restored.content,
        vec![ContentBlock::Text {
            id: ContentBlockId::new("text-0"),
            text: "content before failure".into(),
        }]
    );
}

#[test]
fn agent_failed_partial_usage_replays() {
    // §10.9 failure conformance. Pi basis: packages/agent/src/agent-loop.ts and
    // pi-ai's terminal assistant result retain the latest usage observation.
    let expected_usage = usage(19, 11);
    let failed = assistant_message(
        "assistant-usage",
        "partial",
        AssistantFinishReason::Error,
        expected_usage.clone(),
    );
    let event = AgentEventEnvelope {
        sequence: AGENT_INITIAL_SEQUENCE,
        run_id: RunId::new("run-usage"),
        event: AgentEvent::MessageCommitted {
            message: AgentRecord::Llm(Message::Assistant(failed)),
        },
    };
    let replayed = replay_committed_events(base_state(), [&event]).unwrap();
    let Some(AgentRecord::Llm(Message::Assistant(restored))) = replayed.transcript.last() else {
        panic!("expected assistant record");
    };
    assert_eq!(restored.usage, expected_usage);
}

#[test]
fn agent_event_envelope_and_outcomes_round_trip() {
    // Architecture v2 part 1 §4.4 and part 2 §2.1/§4.4. Pi basis:
    // packages/agent/src/types.ts AgentEvent and agent-loop.ts event ordering.
    let target = ModelFingerprint::new("openai", "openai-responses", "gpt-test");
    let run_id = RunId::new("run-events");
    let events = vec![
        AgentEvent::RunStarted {
            run_id: run_id.clone(),
        },
        AgentEvent::TurnStarted {
            run_id: run_id.clone(),
            turn: 0,
            model: ModelRef::new("openai", "gpt-test"),
        },
        AgentEvent::ContextPrepared {
            turn: 0,
            target: ModelRef::new("openai", "gpt-test"),
            report: HandoffReport::unchanged(target),
        },
        AgentEvent::MessageStarted {
            message_id: MessageId::new("assistant-events"),
            role: MessageRole::Assistant,
        },
        AgentEvent::AssistantUpdate {
            message_id: MessageId::new("assistant-events"),
            event: AssistantEvent::MessageStarted {
                message_id: MessageId::new("assistant-events"),
                provider: ProviderId::new("openai"),
                api: ApiId::new("openai-responses"),
                model: ModelId::new("gpt-test"),
            },
        },
        AgentEvent::MessageCommitted {
            message: user_record(7, "committed"),
        },
        AgentEvent::ToolExecutionStarted {
            call: ToolCall {
                id: ToolCallId::new("call-events"),
                name: "echo".into(),
                arguments: json!({"value": "hello"}),
            },
        },
        AgentEvent::ToolExecutionUpdated {
            call_id: ToolCallId::new("call-events"),
            update: ToolUpdate {
                content: vec![],
                details: None,
                usage: None,
                added_tool_names: vec![],
                terminate: false,
            },
        },
        AgentEvent::ToolExecutionFinished {
            call_id: ToolCallId::new("call-events"),
            result: ToolOutput::new(vec![]),
            is_error: false,
        },
        AgentEvent::TurnFinished {
            outcome: TurnOutcome {
                assistant_message_id: MessageId::new("assistant-events"),
                assistant_finish: AssistantFinishReason::Stop,
                tool_result_message_ids: vec![],
                usage: usage(3, 2),
                cost: Some(Cost {
                    currency: Currency::usd(),
                    micros: 12,
                }),
            },
        },
        AgentEvent::RunFinished {
            outcome: RunOutcome::Completed {
                final_message_id: MessageId::new("assistant-events"),
                usage: usage(3, 2),
                cost: None,
            },
        },
    ];

    for (index, event) in events.into_iter().enumerate() {
        let envelope = AgentEventEnvelope {
            sequence: u64::try_from(index).unwrap() + 1,
            run_id: run_id.clone(),
            event,
        };
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let restored: AgentEventEnvelope = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            panic!(
                "event {index} failed to round-trip: {error}; {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        assert_eq!(restored, envelope);
    }

    for outcome in [
        RunOutcome::Failed {
            committed_message_id: MessageId::new("failed"),
            error: public_error(),
        },
        RunOutcome::Cancelled {
            committed_message_id: MessageId::new("cancelled"),
            reason: CancellationReason::new("request cancelled").with_request_id("request-2"),
        },
    ] {
        let bytes = serde_json::to_vec(&outcome).unwrap();
        let restored: RunOutcome = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored, outcome);
    }
}

#[test]
fn agent_restore_migrates_resolves_binds_and_validates() {
    // Architecture v2 part 1 §4.9 restore order. Pi basis:
    // packages/agent/src/agent.ts constructs configured runtime state around a
    // supplied model, tool list, and transcript.
    let mut state = base_state();
    state
        .transcript
        .push(custom_record("notification", json!({"ok": true})));
    let mut snapshot = AgentSnapshot::new(state);
    snapshot.next_sequence = 17;
    snapshot.streaming = Some(partial_snapshot());
    snapshot.pending_tool_calls = Arc::from([ToolCallId::new("call-pending")]);

    let runtime = Arc::new(ScriptedRuntime::builder().build());
    let mut custom_kinds = CustomRecordKinds::new();
    custom_kinds.register("notification").unwrap();
    let expected = snapshot.clone();
    let agent = Agent::restore(
        snapshot,
        runtime,
        &|model: &ModelRef| model == &ModelRef::new("openai", "gpt-test"),
        ToolRegistry::new(),
        &custom_kinds,
    )
    .unwrap();

    assert_eq!(agent.snapshot(), expected);
    assert!(agent.tools().is_empty());
}

struct NeverLocalRuntime;

impl LocalModelRuntime for NeverLocalRuntime {
    fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        Box::pin(async {
            Err(RequestStartError::new(
                RequestStartErrorKind::RuntimeUnavailable,
                "not called",
            ))
        })
    }
}

#[test]
fn agent_restore_supports_send_and_local_runtime_families() {
    // Architecture v2 part 2 §9.2 requires both object-safe families.
    let snapshot = AgentSnapshot::new(base_state());
    let local = LocalAgent::restore(
        snapshot.clone(),
        Rc::new(NeverLocalRuntime),
        &|_: &ModelRef| true,
        LocalToolRegistry::new(),
        &CustomRecordKinds::new(),
    )
    .unwrap();
    assert_eq!(local.snapshot(), snapshot);

    let send = Agent::restore(
        snapshot.clone(),
        Arc::new(ScriptedRuntime::builder().build()),
        &|_: &ModelRef| true,
        ToolRegistry::new(),
        &CustomRecordKinds::new(),
    )
    .unwrap();
    assert_eq!(send.snapshot(), snapshot);
}

#[test]
fn agent_restore_rejects_schema_model_and_custom_kind_in_order() {
    // Architecture v2 part 1 §4.9: each stage rejects before later stages.
    let mut future = AgentSnapshot::new(base_state());
    future.schema_version += 1;
    let error = Agent::restore(
        future,
        Arc::new(ScriptedRuntime::builder().build()),
        &|_: &ModelRef| panic!("model resolution must follow migration"),
        ToolRegistry::new(),
        &CustomRecordKinds::new(),
    )
    .err()
    .unwrap();
    assert!(matches!(
        error,
        AgentError::UnsupportedSnapshotSchema { .. }
    ));

    let mut custom = base_state();
    custom
        .transcript
        .push(custom_record("missing", json!({"value": 1})));
    let error = Agent::restore(
        AgentSnapshot::new(custom.clone()),
        Arc::new(ScriptedRuntime::builder().build()),
        &|_: &ModelRef| false,
        ToolRegistry::new(),
        &|_: &str| panic!("custom validation must follow model resolution"),
    )
    .err()
    .unwrap();
    assert!(matches!(error, AgentError::UnresolvedModel { .. }));

    let error = Agent::restore(
        AgentSnapshot::new(custom),
        Arc::new(ScriptedRuntime::builder().build()),
        &|_: &ModelRef| true,
        ToolRegistry::new(),
        &CustomRecordKinds::new(),
    )
    .err()
    .unwrap();
    assert_eq!(
        error,
        AgentError::UnknownCustomRecordKind {
            type_name: "missing".into(),
        }
    );
}

#[test]
fn agent_event_replay_rejects_sequence_run_and_message_invariant_violations() {
    // Architecture v2 part 1 §4.3/§8 monotonic envelopes and committed-state
    // replay invariants.
    let run_id = RunId::new("run-invariants");
    let wrong_sequence = AgentEventEnvelope {
        sequence: 2,
        run_id: run_id.clone(),
        event: AgentEvent::RunStarted {
            run_id: run_id.clone(),
        },
    };
    assert!(matches!(
        replay_committed_events(base_state(), [&wrong_sequence]),
        Err(AgentError::EventSequenceMismatch {
            expected: 1,
            actual: 2
        })
    ));

    let wrong_run = AgentEventEnvelope {
        sequence: 1,
        run_id: run_id.clone(),
        event: AgentEvent::RunStarted {
            run_id: RunId::new("other-run"),
        },
    };
    assert!(matches!(
        replay_committed_events(base_state(), [&wrong_run]),
        Err(AgentError::EventRunIdMismatch { .. })
    ));

    let message = user_record(1, "duplicate");
    let duplicate = [
        AgentEventEnvelope {
            sequence: 1,
            run_id: run_id.clone(),
            event: AgentEvent::MessageCommitted {
                message: message.clone(),
            },
        },
        AgentEventEnvelope {
            sequence: 2,
            run_id,
            event: AgentEvent::MessageCommitted { message },
        },
    ];
    assert!(matches!(
        replay_committed_events(base_state(), &duplicate),
        Err(AgentError::DuplicateMessageId { .. })
    ));
}
