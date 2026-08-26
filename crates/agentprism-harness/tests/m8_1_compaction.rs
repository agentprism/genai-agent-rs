use agentprism_ai::{
    ApiId, AssistantFinish, AssistantFinishReason, AssistantMessage, AssistantStream,
    CacheRetention, CancellationToken, ContentBlock, ContentBlockId, Cost, Currency,
    LocalAssistantStream, LocalBoxFuture, LocalModelRuntime, Message, MessageId, ModelId, ModelRef,
    ModelRequest, ModelRuntime, ProviderId, PublicError, ReplayEnvelope, ReplayScope,
    RequestStartError, RunId, SendBoxFuture, SimpleGenerationOptions, Timestamp, ToolCall,
    ToolCallId, Usage, UsageSource, UserMessage,
};
use agentprism_core::{
    AgentRecord, AgentState, AgentStateView, ContextError, ContextPolicy, DefaultContextPolicy,
    LocalContextPolicy, PreparedAgentRecords,
};
use agentprism_env::{Clock, ClockError, LocalClock};
use agentprism_harness::*;
use agentprism_session::{
    CompactionReason, EntryBase, EntryId, InMemorySessionStorage, LaneName, LocalSessionStorage,
    OperationIntent, OperationOutcome, OperationRecord, OperationRecordBase, OperationRecordId,
    OperationStep, ProvisionedEntry, RecoveryDecision, Sequence, SessionEntry,
    SessionEnvironmentMetadata, SessionHeader, SessionMutation, UsageAttribution,
};
use futures_executor::block_on;
use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

#[derive(Clone, Copy)]
struct FixedClock(Timestamp);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(
        &self,
        _duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), ClockError>> {
        Box::pin(async move { cancellation.check().map_err(|_| ClockError::Cancelled) })
    }
}

impl LocalClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(
        &self,
        _duration: Duration,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), ClockError>> {
        Box::pin(async move { cancellation.check().map_err(|_| ClockError::Cancelled) })
    }
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cache_write_one_hour_tokens: None,
        total_tokens: Some(input.saturating_add(output)),
        source: UsageSource::ProviderReported,
    }
}

fn user_record(id: &str, text: &str, timestamp: i64) -> AgentRecord {
    AgentRecord::Llm(Message::User(UserMessage {
        id: MessageId::new(id),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{id}-block")),
            text: text.to_owned(),
        }],
        timestamp: Timestamp::from_unix_millis(timestamp),
    }))
}

fn message_text(message: &Message) -> String {
    match message {
        Message::User(message) => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::Assistant(_) | Message::ToolResult(_) => String::new(),
    }
}

fn assistant_message(
    id: &str,
    text: &str,
    finish_reason: AssistantFinishReason,
    error: Option<PublicError>,
    usage: Usage,
) -> AssistantMessage {
    let provider = ProviderId::new("test");
    let api = ApiId::new("test-api");
    let model = ModelId::new("model");
    AssistantMessage {
        id: MessageId::new(id),
        provider: provider.clone(),
        api: api.clone(),
        requested_model: model.clone(),
        response_model: None,
        response_id: None,
        deferred: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: (!text.is_empty())
            .then(|| ContentBlock::Text {
                id: ContentBlockId::new(format!("{id}-block")),
                text: text.to_owned(),
            })
            .into_iter()
            .collect(),
        replay: ReplayEnvelope::new(ReplayScope {
            provider,
            api,
            requested_model: model.clone(),
            produced_by_model: model,
            protocol_revision: None,
        }),
        usage,
        cost: Some(Cost {
            currency: Currency::usd(),
            micros: 1,
        }),
        finish: AssistantFinish {
            reason: finish_reason,
            raw_provider_reason: None,
            error,
        },
        timestamp: Timestamp::from_unix_millis(10),
    }
}

fn storage() -> Arc<InMemorySessionStorage> {
    Arc::new(
        InMemorySessionStorage::new(SessionHeader::new(
            "session",
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .expect("valid test header"),
    )
}

fn harness_session(storage: Arc<InMemorySessionStorage>, lane: &str) -> Arc<Session> {
    Arc::new(Session::new(
        storage,
        LaneName::new(lane),
        Arc::new(MonotonicHarnessIdGenerator::new("test")),
        Arc::new(FixedClock(Timestamp::from_unix_millis(100))),
    ))
}

fn local_storage() -> Rc<InMemorySessionStorage> {
    Rc::new(
        InMemorySessionStorage::new(SessionHeader::new(
            "local-session",
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .expect("valid local test header"),
    )
}

fn local_harness_session(storage: Rc<InMemorySessionStorage>, lane: &str) -> Rc<LocalSession> {
    Rc::new(LocalSession::new(
        storage,
        LaneName::new(lane),
        Rc::new(MonotonicHarnessIdGenerator::new("local-test")),
        Rc::new(FixedClock(Timestamp::from_unix_millis(100))),
    ))
}

struct RcOnlyStorage {
    inner: InMemorySessionStorage,
    _not_send: Rc<()>,
}

impl RcOnlyStorage {
    fn new() -> Self {
        Self {
            inner: InMemorySessionStorage::new(SessionHeader::new(
                "rc-only-session",
                Timestamp::from_unix_millis(1),
                SessionEnvironmentMetadata::default(),
            ))
            .expect("valid Rc-only header"),
            _not_send: Rc::new(()),
        }
    }
}

impl LocalSessionStorage for RcOnlyStorage {
    fn metadata(
        &self,
    ) -> LocalBoxFuture<
        '_,
        Result<agentprism_session::SessionMetadata, agentprism_session::SessionError>,
    > {
        LocalSessionStorage::metadata(&self.inner)
    }

    fn load_state(
        &self,
    ) -> LocalBoxFuture<
        '_,
        Result<agentprism_session::SessionState, agentprism_session::SessionError>,
    > {
        LocalSessionStorage::load_state(&self.inner)
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> LocalBoxFuture<
        '_,
        Result<agentprism_session::AppendReceipt, agentprism_session::SessionError>,
    > {
        LocalSessionStorage::append(&self.inner, expected_sequence, mutations)
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMutation>, agentprism_session::SessionError>> {
        LocalSessionStorage::log(&self.inner, after, limit)
    }

    fn repair_tail(
        &self,
    ) -> LocalBoxFuture<
        '_,
        Result<agentprism_session::TailRepairReport, agentprism_session::SessionError>,
    > {
        LocalSessionStorage::repair_tail(&self.inner)
    }
}

fn rc_only_harness_session(storage: Rc<RcOnlyStorage>, lane: &str) -> Rc<LocalSession> {
    Rc::new(LocalSession::new(
        storage,
        LaneName::new(lane),
        Rc::new(MonotonicHarnessIdGenerator::new("rc-only")),
        Rc::new(FixedClock(Timestamp::from_unix_millis(100))),
    ))
}

fn append_message(
    storage: &InMemorySessionStorage,
    lane: &str,
    id: &str,
    parent_id: Option<&str>,
    record: AgentRecord,
) {
    let state = storage.state_snapshot().expect("state snapshot");
    let sequence = state.next_sequence().expect("sequence");
    storage
        .append_batch(
            state.sequence(),
            vec![SessionMutation::Entry {
                lane: Some(LaneName::new(lane)),
                entry: SessionEntry::Message {
                    base: EntryBase {
                        id: EntryId::new(id),
                        sequence,
                        parent_id: parent_id.map(EntryId::new),
                        timestamp: Timestamp::from_unix_millis(
                            i64::try_from(sequence.get()).unwrap(),
                        ),
                    },
                    message: record,
                    terminate: false,
                },
            }],
        )
        .expect("append message");
}

fn append_compaction(
    storage: &InMemorySessionStorage,
    lane: &str,
    id: &str,
    parent_id: Option<&str>,
) {
    let state = storage.state_snapshot().expect("state snapshot");
    let sequence = state.next_sequence().expect("sequence");
    storage
        .append_batch(
            state.sequence(),
            vec![SessionMutation::Entry {
                lane: Some(LaneName::new(lane)),
                entry: SessionEntry::Compaction {
                    base: EntryBase {
                        id: EntryId::new(id),
                        sequence,
                        parent_id: parent_id.map(EntryId::new),
                        timestamp: Timestamp::from_unix_millis(
                            i64::try_from(sequence.get()).unwrap(),
                        ),
                    },
                    summary: "existing summary".to_owned(),
                    retained_tail: Vec::new(),
                    tokens_before: 100,
                    details: None,
                    usage: None,
                },
            }],
        )
        .expect("append compaction");
}

fn append_reasoning_change(
    storage: &InMemorySessionStorage,
    lane: &str,
    id: &str,
    parent_id: Option<&str>,
    level: agentprism_ai::ReasoningLevel,
) {
    let state = storage.state_snapshot().expect("state snapshot");
    let sequence = state.next_sequence().expect("sequence");
    storage
        .append_batch(
            state.sequence(),
            vec![SessionMutation::Entry {
                lane: Some(LaneName::new(lane)),
                entry: SessionEntry::ReasoningChange {
                    base: EntryBase {
                        id: EntryId::new(id),
                        sequence,
                        parent_id: parent_id.map(EntryId::new),
                        timestamp: Timestamp::from_unix_millis(
                            i64::try_from(sequence.get()).unwrap(),
                        ),
                    },
                    level,
                },
            }],
        )
        .expect("append reasoning change");
}

fn append_custom_entry(
    storage: &InMemorySessionStorage,
    lane: &str,
    id: &str,
    parent_id: Option<&str>,
    custom_type: &str,
    value: serde_json::Value,
) {
    let state = storage.state_snapshot().expect("state snapshot");
    let sequence = state.next_sequence().expect("sequence");
    storage
        .append_batch(
            state.sequence(),
            vec![SessionMutation::Entry {
                lane: Some(LaneName::new(lane)),
                entry: SessionEntry::Custom {
                    base: EntryBase {
                        id: EntryId::new(id),
                        sequence,
                        parent_id: parent_id.map(EntryId::new),
                        timestamp: Timestamp::from_unix_millis(
                            i64::try_from(sequence.get()).unwrap(),
                        ),
                    },
                    custom_type: custom_type.to_owned(),
                    data: Some(agentprism_ai::VersionedExtension {
                        schema_version: 1,
                        value: serde_json::value::to_raw_value(&value).unwrap(),
                    }),
                },
            }],
        )
        .expect("append custom entry");
}

fn create_lane(storage: &InMemorySessionStorage, lane: &str, leaf: &str) {
    let state = storage.state_snapshot().expect("state snapshot");
    storage
        .append_batch(
            state.sequence(),
            vec![SessionMutation::Lane {
                sequence: state.next_sequence().expect("sequence"),
                lane: LaneName::new(lane),
                leaf_id: Some(EntryId::new(leaf)),
            }],
        )
        .expect("create lane");
}

fn operation_base(id: &str, lane: &str, sequence: Sequence) -> OperationRecordBase {
    OperationRecordBase {
        id: OperationRecordId::new(id),
        sequence,
        lane: LaneName::new(lane),
        timestamp: Timestamp::from_unix_millis(i64::try_from(sequence.get()).unwrap()),
    }
}

fn append_operation_started(
    storage: &InMemorySessionStorage,
    lane: &str,
    run_id: &str,
    source_leaf_id: Option<&str>,
    intent: OperationIntent,
) {
    let state = storage.state_snapshot().expect("state snapshot");
    let sequence = state.next_sequence().expect("sequence");
    storage
        .append_batch(
            state.sequence(),
            vec![SessionMutation::Record {
                record: OperationRecord::Started {
                    base: operation_base(run_id, lane, sequence),
                    source_leaf_id: source_leaf_id.map(EntryId::new),
                    intent,
                },
            }],
        )
        .expect("append operation start");
}

fn agent_state() -> AgentState {
    AgentState::new(
        "system",
        ModelRef::new("test", "model"),
        agentprism_ai::ReasoningLevel::Off,
    )
}

fn state_view<'a>(state: &'a AgentState) -> AgentStateView<'a> {
    static OPTIONS: OnceLock<SimpleGenerationOptions> = OnceLock::new();
    AgentStateView {
        state,
        records: &state.transcript,
        tools: &[],
        model: &state.model,
        reasoning: state.reasoning,
        options: OPTIONS.get_or_init(SimpleGenerationOptions::default),
    }
}

fn state_view_with_options<'a>(
    state: &'a AgentState,
    options: &'a SimpleGenerationOptions,
) -> AgentStateView<'a> {
    AgentStateView {
        state,
        records: &state.transcript,
        tools: &[],
        model: &state.model,
        reasoning: state.reasoning,
        options,
    }
}

struct FixedCompactionPolicy {
    fail_count: Mutex<usize>,
    observed_reasons: Mutex<Vec<Option<CompactionReason>>>,
    result_usage: Usage,
    only_forced: bool,
}

struct RecordingRuntime {
    scripted: agentprism_ai::ScriptedRuntime,
    requests: Mutex<Vec<ModelRequest>>,
}

impl RecordingRuntime {
    fn new(responses: impl IntoIterator<Item = agentprism_ai::ScriptedResponse>) -> Self {
        Self {
            scripted: agentprism_ai::ScriptedRuntime::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl ModelRuntime for RecordingRuntime {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, RequestStartError>> {
        self.requests.lock().unwrap().push(request.clone());
        ModelRuntime::stream(&self.scripted, request, cancellation)
    }
}

impl LocalModelRuntime for RecordingRuntime {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
        self.requests.lock().unwrap().push(request.clone());
        LocalModelRuntime::stream(&self.scripted, request, cancellation)
    }
}

impl FixedCompactionPolicy {
    fn succeeding() -> Self {
        Self {
            fail_count: Mutex::new(0),
            observed_reasons: Mutex::new(Vec::new()),
            result_usage: usage(7, 3),
            only_forced: false,
        }
    }

    fn fail_once() -> Self {
        Self {
            fail_count: Mutex::new(1),
            ..Self::succeeding()
        }
    }

    fn only_forced() -> Self {
        Self {
            only_forced: true,
            ..Self::succeeding()
        }
    }
}

impl CompactionPolicy for FixedCompactionPolicy {
    fn decide(
        &self,
        input: CompactionDecisionInput<'_>,
    ) -> Result<CompactionDecision, CompactionError> {
        self.observed_reasons
            .lock()
            .expect("reason lock")
            .push(input.requested_reason);
        let Some(reason) = input
            .requested_reason
            .or((!self.only_forced).then_some(CompactionReason::Threshold))
        else {
            return Ok(CompactionDecision::NoCompaction);
        };
        Ok(CompactionDecision::Compact {
            reason,
            retained_tail_start: input.records.len().saturating_sub(1),
            summary_model: ModelRef::new("test", "summary"),
        })
    }

    fn compact(
        &self,
        input: CompactionInput,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<CompactionResult, CompactionError>> {
        Box::pin(async move {
            let mut failures = self.fail_count.lock().expect("failure lock");
            if *failures > 0 {
                *failures -= 1;
                return Err(CompactionError::summarization("scripted summary failure"));
            }
            drop(failures);
            Ok(CompactionResult {
                summary: format!("summary for {:?}", input.reason),
                retained_tail: input.records[input.retained_tail_start..].to_vec(),
                tokens_before: input.tokens_before,
                details: None,
                usage: Some(self.result_usage.clone()),
                cost: None,
                stop_reason: AssistantFinishReason::Stop,
            })
        })
    }
}

impl LocalCompactionPolicy for FixedCompactionPolicy {
    fn decide(
        &self,
        input: CompactionDecisionInput<'_>,
    ) -> Result<CompactionDecision, CompactionError> {
        self.observed_reasons
            .lock()
            .expect("reason lock")
            .push(input.requested_reason);
        let Some(reason) = input
            .requested_reason
            .or((!self.only_forced).then_some(CompactionReason::Threshold))
        else {
            return Ok(CompactionDecision::NoCompaction);
        };
        Ok(CompactionDecision::Compact {
            reason,
            retained_tail_start: input.records.len().saturating_sub(1),
            summary_model: ModelRef::new("test", "summary"),
        })
    }

    fn compact(
        &self,
        input: CompactionInput,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<CompactionResult, CompactionError>> {
        Box::pin(async move {
            let mut failures = self.fail_count.lock().expect("failure lock");
            if *failures > 0 {
                *failures -= 1;
                return Err(CompactionError::summarization("scripted summary failure"));
            }
            drop(failures);
            Ok(CompactionResult {
                summary: format!("summary for {:?}", input.reason),
                retained_tail: input.records[input.retained_tail_start..].to_vec(),
                tokens_before: input.tokens_before,
                details: None,
                usage: Some(self.result_usage.clone()),
                cost: None,
                stop_reason: AssistantFinishReason::Stop,
            })
        })
    }
}

struct ExplicitCustomProjector;

impl CustomSessionEntryProjector for ExplicitCustomProjector {
    fn project(
        &self,
        entry: &SessionEntry,
        _index: usize,
        _path: &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError> {
        let SessionEntry::Custom { base, data, .. } = entry else {
            return Ok(Vec::new());
        };
        let text = data
            .as_ref()
            .map_or("null", |extension| extension.value.get());
        Ok(vec![user_record(
            &format!("{}-projected", base.id),
            text,
            base.timestamp.unix_millis(),
        )])
    }
}

#[derive(Clone, Default)]
struct ObservingCustomProjector {
    observations: ProjectorObservations,
}

type ProjectorObservations = Arc<Mutex<Vec<(usize, Vec<String>)>>>;

impl ObservingCustomProjector {
    fn project_entry(
        &self,
        entry: &SessionEntry,
        index: usize,
        path: &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError> {
        self.observations.lock().unwrap().push((
            index,
            path.iter()
                .map(|candidate| candidate.base().id.as_str().to_owned())
                .collect(),
        ));
        ExplicitCustomProjector.project(entry, index, path)
    }
}

impl CustomSessionEntryProjector for ObservingCustomProjector {
    fn project(
        &self,
        entry: &SessionEntry,
        index: usize,
        path: &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError> {
        self.project_entry(entry, index, path)
    }
}

impl LocalCustomSessionEntryProjector for ObservingCustomProjector {
    fn project(
        &self,
        entry: &SessionEntry,
        index: usize,
        path: &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError> {
        self.project_entry(entry, index, path)
    }
}

#[derive(Clone, Default)]
struct AuthoritativeOptionsContextPolicy {
    observations: ReasoningObservations,
}

type ReasoningObservations = Arc<
    Mutex<
        Vec<(
            agentprism_ai::ReasoningLevel,
            Option<agentprism_ai::ReasoningLevel>,
        )>,
    >,
>;

impl AuthoritativeOptionsContextPolicy {
    fn prepare(&self, state: AgentStateView<'_>) -> PreparedAgentRecords {
        self.observations
            .lock()
            .unwrap()
            .push((state.reasoning, state.options.reasoning));
        let mut options = state.options.clone();
        options.reasoning = Some(agentprism_ai::ReasoningLevel::Low);
        options.seed = Some(9_001);
        PreparedAgentRecords {
            records: state.records.to_vec(),
            model_override: None,
            options_override: Some(options),
            report: None,
        }
    }
}

impl ContextPolicy for AuthoritativeOptionsContextPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move { Ok(self.prepare(state)) })
    }
}

impl LocalContextPolicy for AuthoritativeOptionsContextPolicy {
    fn prepare_agent_records<'a>(
        &'a self,
        state: AgentStateView<'a>,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'a, Result<PreparedAgentRecords, ContextError>> {
        Box::pin(async move { Ok(self.prepare(state)) })
    }
}

fn context_policy(
    session: Arc<Session>,
    policy: Arc<dyn CompactionPolicy>,
) -> HarnessContextPolicy {
    HarnessContextPolicy {
        base: Arc::new(DefaultContextPolicy),
        compaction: policy,
        session,
        custom_entry_projector: Arc::new(OmitCustomSessionEntries),
    }
}

fn local_context_policy(
    session: Rc<LocalSession>,
    policy: Rc<dyn LocalCompactionPolicy>,
) -> LocalHarnessContextPolicy {
    LocalHarnessContextPolicy {
        base: Rc::new(DefaultContextPolicy),
        compaction: policy,
        session,
        custom_entry_projector: Rc::new(OmitCustomSessionEntries),
    }
}

#[test]
fn compaction_threshold_decision() {
    // Pi basis: packages/agent/test/harness/compaction.test.ts, "checks compaction threshold".
    let runtime = Arc::new(agentprism_ai::ScriptedRuntime::new([]));
    let policy = RuntimeCompactionPolicy::new(
        runtime,
        ModelRef::new("test", "summary"),
        100,
        32,
        false,
        CompactionSettings {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 10,
        },
    );
    let records = vec![user_record("u", "hello", 1)];
    let compact = policy
        .decide(CompactionDecisionInput {
            records: &records,
            structural_branch_summary_indices: &[],
            context_tokens: 91,
            context_window: 100,
            requested_reason: None,
            current_model: &ModelRef::new("test", "model"),
        })
        .expect("decision");
    assert!(matches!(
        compact,
        CompactionDecision::Compact {
            reason: CompactionReason::Threshold,
            ..
        }
    ));
    assert_eq!(
        policy
            .decide(CompactionDecisionInput {
                records: &records,
                structural_branch_summary_indices: &[],
                context_tokens: 90,
                context_window: 100,
                requested_reason: None,
                current_model: &ModelRef::new("test", "model"),
            })
            .expect("decision"),
        CompactionDecision::NoCompaction
    );
}

#[test]
fn compaction_manual_reason() {
    // Pi basis: packages/agent/test/harness/reducer.test.ts, "manual compaction" and
    // "threshold auto-compaction" valid prefixes.
    block_on(async {
        let primary_storage = storage();
        append_message(
            &primary_storage,
            "main",
            "u1",
            None,
            user_record("u1", "old", 1),
        );
        let policy = Arc::new(FixedCompactionPolicy::succeeding());
        context_policy(
            harness_session(primary_storage.clone(), "main"),
            policy.clone(),
        )
        .compact_manual(state_view(&agent_state()), None, CancellationToken::new())
        .await
        .expect("manual compaction");
        assert_eq!(
            policy.observed_reasons.lock().unwrap().as_slice(),
            &[Some(CompactionReason::Manual)]
        );
        assert!(
            primary_storage
                .state_snapshot()
                .unwrap()
                .records_in_sequence_order()
                .iter()
                .any(|record| matches!(
                    record,
                    OperationRecord::StepAttempt {
                        compaction_reason: Some(CompactionReason::Manual),
                        ..
                    }
                ))
        );

        let busy_storage = storage();
        append_message(
            &busy_storage,
            "main",
            "busy-user",
            None,
            user_record("busy-user", "busy", 1),
        );
        append_operation_started(
            &busy_storage,
            "main",
            "busy-run",
            Some("busy-user"),
            OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: Default::default(),
            },
        );
        let busy_harness = context_policy(
            harness_session(busy_storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::succeeding()),
        );
        assert!(
            busy_harness
                .compact_manual(state_view(&agent_state()), None, CancellationToken::new())
                .await
                .is_err(),
            "manual compaction must not borrow an open run"
        );
        let local_busy_storage = local_storage();
        append_message(
            &local_busy_storage,
            "main",
            "local-busy-user",
            None,
            user_record("local-busy-user", "busy", 1),
        );
        append_operation_started(
            &local_busy_storage,
            "main",
            "local-busy-run",
            Some("local-busy-user"),
            OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: Default::default(),
            },
        );
        assert!(
            local_context_policy(
                local_harness_session(local_busy_storage, "main"),
                Rc::new(FixedCompactionPolicy::succeeding()),
            )
            .compact_manual(state_view(&agent_state()), None, CancellationToken::new())
            .await
            .is_err(),
            "local manual compaction must not borrow an open run"
        );
        assert_eq!(
            busy_storage
                .state_snapshot()
                .unwrap()
                .records_in_sequence_order()
                .iter()
                .filter(|record| matches!(record, OperationRecord::Started { .. }))
                .count(),
            1
        );

        let threshold_storage = storage();
        append_message(
            &threshold_storage,
            "main",
            "threshold-user",
            None,
            user_record("threshold-user", "threshold", 1),
        );
        append_operation_started(
            &threshold_storage,
            "main",
            "threshold-run",
            Some("threshold-user"),
            OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: Default::default(),
            },
        );
        context_policy(
            harness_session(threshold_storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::succeeding()),
        )
        .prepare_agent_records(state_view(&agent_state()), CancellationToken::new())
        .await
        .expect("threshold compaction reuses the open run");
        let threshold_state = threshold_storage.state_snapshot().unwrap();
        assert_eq!(
            threshold_state
                .open_operations(&LaneName::new("main"))
                .len(),
            1
        );
        assert!(threshold_state.records_in_sequence_order().iter().any(|record| {
            matches!(record, OperationRecord::StepAttempt { run_id, step: OperationStep::Compaction, .. } if *run_id == RunId::new("threshold-run"))
        }));

        let navigation_storage = storage();
        append_message(
            &navigation_storage,
            "main",
            "navigation-user",
            None,
            user_record("navigation-user", "navigation", 1),
        );
        append_operation_started(
            &navigation_storage,
            "main",
            "navigation-run",
            Some("navigation-user"),
            OperationIntent::Navigation {
                target_id: Some(EntryId::new("navigation-user")),
                summarize: false,
                custom_instructions: None,
                label: None,
                summary_entry_id: None,
            },
        );
        assert!(
            context_policy(
                harness_session(navigation_storage.clone(), "main"),
                Arc::new(FixedCompactionPolicy::succeeding()),
            )
            .prepare_agent_records(state_view(&agent_state()), CancellationToken::new())
            .await
            .is_err(),
            "threshold compaction must never borrow a navigation operation"
        );
        let local_navigation_storage = local_storage();
        append_message(
            &local_navigation_storage,
            "main",
            "local-navigation-user",
            None,
            user_record("local-navigation-user", "navigation", 1),
        );
        append_operation_started(
            &local_navigation_storage,
            "main",
            "local-navigation-run",
            Some("local-navigation-user"),
            OperationIntent::Navigation {
                target_id: Some(EntryId::new("local-navigation-user")),
                summarize: false,
                custom_instructions: None,
                label: None,
                summary_entry_id: None,
            },
        );
        assert!(
            agentprism_core::LocalContextPolicy::prepare_agent_records(
                &local_context_policy(
                    local_harness_session(local_navigation_storage, "main"),
                    Rc::new(FixedCompactionPolicy::succeeding()),
                ),
                state_view(&agent_state()),
                CancellationToken::new(),
            )
            .await
            .is_err(),
            "local threshold compaction must never borrow a navigation operation"
        );
        assert!(
            !navigation_storage
                .state_snapshot()
                .unwrap()
                .records_in_sequence_order()
                .iter()
                .any(|record| matches!(record, OperationRecord::StepAttempt { .. }))
        );
    });
}

#[test]
fn compaction_last_entry_suppresses_manual_and_forced_requests() {
    // Pi basis: packages/agent/src/harness/compaction/compaction.ts,
    // prepareCompaction returns undefined whenever the path tail is a compaction entry.
    block_on(async {
        let send_manual_storage = storage();
        append_message(
            &send_manual_storage,
            "main",
            "send-manual-user",
            None,
            user_record("send-manual-user", "old", 1),
        );
        append_compaction(
            &send_manual_storage,
            "main",
            "send-manual-compaction",
            Some("send-manual-user"),
        );
        let send_manual_policy = Arc::new(FixedCompactionPolicy::succeeding());
        let send_manual = context_policy(
            harness_session(send_manual_storage.clone(), "main"),
            send_manual_policy.clone(),
        )
        .compact_manual(state_view(&agent_state()), None, CancellationToken::new())
        .await
        .expect("manual preparation after a compaction tail");
        assert_eq!(send_manual.compaction_entry_id, None);
        assert!(
            send_manual_policy
                .observed_reasons
                .lock()
                .unwrap()
                .is_empty()
        );

        let local_manual_storage = local_storage();
        append_message(
            &local_manual_storage,
            "main",
            "local-manual-user",
            None,
            user_record("local-manual-user", "old", 1),
        );
        append_compaction(
            &local_manual_storage,
            "main",
            "local-manual-compaction",
            Some("local-manual-user"),
        );
        let local_manual_policy = Rc::new(FixedCompactionPolicy::succeeding());
        let local_manual = local_context_policy(
            local_harness_session(local_manual_storage.clone(), "main"),
            local_manual_policy.clone(),
        )
        .compact_manual(state_view(&agent_state()), None, CancellationToken::new())
        .await
        .expect("local manual preparation after a compaction tail");
        assert_eq!(local_manual.compaction_entry_id, None);
        assert!(
            local_manual_policy
                .observed_reasons
                .lock()
                .unwrap()
                .is_empty()
        );

        let send_forced_storage = storage();
        append_message(
            &send_forced_storage,
            "main",
            "send-forced-user",
            None,
            user_record("send-forced-user", "large prompt", 1),
        );
        append_compaction(
            &send_forced_storage,
            "main",
            "send-forced-compaction",
            Some("send-forced-user"),
        );
        let send_forced_session = harness_session(send_forced_storage.clone(), "main");
        let send_forced_policy = Arc::new(FixedCompactionPolicy::succeeding());
        let send_forced_context = Arc::new(context_policy(
            send_forced_session.clone(),
            send_forced_policy.clone(),
        ));
        let send_step = QueueAssistantStep(Mutex::new(VecDeque::from([
            explicit_overflow_message("send-forced-overflow"),
            assistant_message(
                "send-forced-success",
                "fits",
                AssistantFinishReason::Stop,
                None,
                usage(5, 1),
            ),
        ])));
        OverflowRetryExecutor {
            context_policy: send_forced_context,
            session: send_forced_session,
            context_window: Some(100),
        }
        .run(
            state_view(&agent_state()),
            OverflowRunIntent::default(),
            &send_step,
            CancellationToken::new(),
        )
        .await
        .expect("forced overflow request after a compaction tail");
        assert!(
            send_forced_policy
                .observed_reasons
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(
            !send_forced_storage
                .state_snapshot()
                .unwrap()
                .records_in_sequence_order()
                .iter()
                .any(|record| matches!(
                    record,
                    OperationRecord::StepAttempt {
                        step: OperationStep::Compaction,
                        ..
                    }
                ))
        );

        let local_forced_storage = local_storage();
        append_message(
            &local_forced_storage,
            "main",
            "local-forced-user",
            None,
            user_record("local-forced-user", "large prompt", 1),
        );
        append_compaction(
            &local_forced_storage,
            "main",
            "local-forced-compaction",
            Some("local-forced-user"),
        );
        let local_forced_session = local_harness_session(local_forced_storage.clone(), "main");
        let local_forced_policy = Rc::new(FixedCompactionPolicy::succeeding());
        let local_forced_context = Rc::new(local_context_policy(
            local_forced_session.clone(),
            local_forced_policy.clone(),
        ));
        let local_step = LocalQueueAssistantStep(RefCell::new(VecDeque::from([
            explicit_overflow_message("local-forced-overflow"),
            assistant_message(
                "local-forced-success",
                "fits",
                AssistantFinishReason::Stop,
                None,
                usage(5, 1),
            ),
        ])));
        LocalOverflowRetryExecutor {
            context_policy: local_forced_context,
            session: local_forced_session,
            context_window: Some(100),
        }
        .run(
            state_view(&agent_state()),
            OverflowRunIntent::default(),
            &local_step,
            CancellationToken::new(),
        )
        .await
        .expect("local forced overflow request after a compaction tail");
        assert!(
            local_forced_policy
                .observed_reasons
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(
            !local_forced_storage
                .state_snapshot()
                .unwrap()
                .records_in_sequence_order()
                .iter()
                .any(|record| matches!(
                    record,
                    OperationRecord::StepAttempt {
                        step: OperationStep::Compaction,
                        ..
                    }
                ))
        );
    });
}

#[test]
fn compaction_projection_empty_nonempty_path_invokes_summary_send() {
    // Pi basis: packages/agent/src/harness/compaction/compaction.ts `prepareCompaction`
    // suppresses an empty path, but `compact` still summarizes a nonempty path whose
    // model/reasoning/custom entries project to no messages.
    block_on(async {
        let send_storage = storage();
        let runtime = Arc::new(RecordingRuntime::new([agentprism_ai::text_response(
            "empty projection summary",
        )]));
        let policy = Arc::new(RuntimeCompactionPolicy::new(
            runtime.clone(),
            ModelRef::new("test", "summary"),
            100_000,
            2_048,
            false,
            CompactionSettings::default(),
        ));
        let context = context_policy(harness_session(send_storage.clone(), "main"), policy);
        let state = agent_state();

        let empty = context
            .compact_manual(state_view(&state), None, CancellationToken::new())
            .await
            .expect("empty path is not compacted");
        assert_eq!(empty.compaction_entry_id, None);
        assert!(runtime.requests.lock().unwrap().is_empty());

        append_reasoning_change(
            &send_storage,
            "main",
            "send-reasoning-only",
            None,
            agentprism_ai::ReasoningLevel::High,
        );
        let compacted = context
            .compact_manual(state_view(&state), None, CancellationToken::new())
            .await
            .expect("nonempty projection-empty path is compacted");
        assert!(compacted.compaction_entry_id.is_some());

        let requests = runtime.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            message_text(&requests[0].context.messages[0])
                .contains("<conversation>\n\n</conversation>")
        );
    });
}

#[test]
fn compaction_projection_empty_nonempty_path_invokes_summary_local() {
    // Pi basis: packages/agent/src/harness/compaction/compaction.ts `prepareCompaction`
    // and `compact`; Architecture v2 part 2 §9.2 requires the Local policy family
    // to preserve the same path-versus-projection distinction.
    block_on(async {
        let local_storage = local_storage();
        let runtime = Rc::new(RecordingRuntime::new([agentprism_ai::text_response(
            "local empty projection summary",
        )]));
        let policy = Rc::new(LocalRuntimeCompactionPolicy::new(
            runtime.clone(),
            ModelRef::new("test", "summary"),
            100_000,
            2_048,
            false,
            CompactionSettings::default(),
        ));
        let context =
            local_context_policy(local_harness_session(local_storage.clone(), "main"), policy);
        let state = agent_state();

        let empty = context
            .compact_manual(state_view(&state), None, CancellationToken::new())
            .await
            .expect("local empty path is not compacted");
        assert_eq!(empty.compaction_entry_id, None);
        assert!(runtime.requests.lock().unwrap().is_empty());

        append_reasoning_change(
            &local_storage,
            "main",
            "local-reasoning-only",
            None,
            agentprism_ai::ReasoningLevel::High,
        );
        let compacted = context
            .compact_manual(state_view(&state), None, CancellationToken::new())
            .await
            .expect("local nonempty projection-empty path is compacted");
        assert!(compacted.compaction_entry_id.is_some());

        let requests = runtime.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            message_text(&requests[0].context.messages[0])
                .contains("<conversation>\n\n</conversation>")
        );
    });
}

#[test]
fn compaction_retains_configured_tail() {
    // Pi basis: packages/agent/test/harness/compaction.test.ts, cut-point and retained-tail tests.
    // Pi basis: compaction.ts `findTurnStartIndex`, where bashExecution starts a turn.
    // Pi basis: compaction.ts `findValidCutPoints`, which excludes unknown augmented roles.
    block_on(async {
        let records = vec![
            user_record("u1", &"a".repeat(80), 1),
            user_record("u2", &"b".repeat(80), 2),
            user_record("u3", &"c".repeat(8), 3),
        ];
        assert_eq!(find_retained_tail_start(&records, 20).unwrap(), 1);

        let unknown_role = AgentRecord::Custom {
            type_name: "unknownAugmentedRole".to_owned(),
            payload: serde_json::value::to_raw_value(&serde_json::json!({
                "content": "not a Pi cut point",
                "timestamp": 1
            }))
            .unwrap(),
        };
        assert_eq!(
            find_retained_tail_start(
                &[unknown_role, user_record("known-cut", "retained", 2)],
                u64::MAX,
            )
            .unwrap(),
            1,
            "an unknown augmented role must not become the fallback cut point",
        );

        let runtime = Arc::new(RecordingRuntime::new([
            agentprism_ai::text_response("history"),
            agentprism_ai::text_response("bash prefix"),
        ]));
        let policy = RuntimeCompactionPolicy::new(
            runtime.clone(),
            ModelRef::new("test", "summary"),
            100_000,
            2_048,
            false,
            CompactionSettings {
                keep_recent_tokens: 20,
                ..CompactionSettings::default()
            },
        );
        let structural_summary = AgentRecord::Custom {
            type_name: "branchSummary".to_owned(),
            payload: serde_json::value::to_raw_value(&serde_json::json!({
                "summary": "structural branch boundary",
                "fromId": "abandoned",
                "timestamp": 2
            }))
            .unwrap(),
        };
        let structural_tail = AgentRecord::Llm(Message::Assistant(assistant_message(
            "structural-retained",
            "retained suffix",
            AssistantFinishReason::Stop,
            None,
            usage(1, 1),
        )));
        let structural_records = vec![
            user_record("structural-history", "older turn", 1),
            structural_summary.clone(),
            structural_tail.clone(),
        ];
        let structural_runtime = Arc::new(RecordingRuntime::new([agentprism_ai::text_response(
            "structural history",
        )]));
        let structural_policy = RuntimeCompactionPolicy::new(
            structural_runtime.clone(),
            ModelRef::new("test", "summary"),
            100_000,
            2_048,
            false,
            CompactionSettings {
                keep_recent_tokens: 1,
                ..CompactionSettings::default()
            },
        );
        let current_model = ModelRef::new("test", "model");
        let structural_decision = structural_policy
            .decide(CompactionDecisionInput {
                records: &structural_records,
                structural_branch_summary_indices: &[1],
                context_tokens: 1,
                context_window: 100_000,
                requested_reason: Some(CompactionReason::Manual),
                current_model: &current_model,
            })
            .expect("structural branch-summary cut decision");
        let CompactionDecision::Compact {
            retained_tail_start,
            ..
        } = structural_decision
        else {
            panic!("manual structural compaction must select a cut");
        };
        assert_eq!(retained_tail_start, 1);
        let structural_compacted = structural_policy
            .compact(
                CompactionInput {
                    records: structural_records,
                    structural_branch_summary_indices: vec![1],
                    retained_tail_start,
                    tokens_before: 10,
                    reason: CompactionReason::Manual,
                    summary_model: ModelRef::new("test", "summary"),
                    result_entry_id: EntryId::new("structural-turn-cut"),
                    previous_summary: None,
                    previous_details: None,
                    custom_instructions: None,
                    reasoning: agentprism_ai::ReasoningLevel::Off,
                    timestamp: Timestamp::from_unix_millis(10),
                },
                CancellationToken::new(),
            )
            .await
            .expect("structural branch-summary boundary compaction");
        assert_eq!(
            structural_compacted.retained_tail,
            vec![structural_summary, structural_tail]
        );
        {
            let structural_requests = structural_runtime.requests.lock().unwrap();
            assert_eq!(structural_requests.len(), 1);
            let structural_history = message_text(&structural_requests[0].context.messages[0]);
            assert!(structural_history.contains("[User]: older turn"));
            assert!(!structural_history.contains("structural branch boundary"));
        }

        let bash = AgentRecord::Custom {
            type_name: "bashExecution".to_owned(),
            payload: serde_json::value::to_raw_value(&serde_json::json!({
                "command": "cargo test",
                "output": "ok",
                "exitCode": 0,
                "cancelled": false,
                "truncated": false
            }))
            .unwrap(),
        };
        let assistant = AgentRecord::Llm(Message::Assistant(assistant_message(
            "after-bash",
            "done",
            AssistantFinishReason::Stop,
            None,
            usage(1, 1),
        )));
        let compacted = policy
            .compact(
                CompactionInput {
                    records: vec![user_record("old", "old turn", 1), bash, assistant.clone()],
                    structural_branch_summary_indices: Vec::new(),
                    retained_tail_start: 2,
                    tokens_before: 10,
                    reason: CompactionReason::Manual,
                    summary_model: ModelRef::new("test", "summary"),
                    result_entry_id: EntryId::new("bash-cut"),
                    previous_summary: None,
                    previous_details: None,
                    custom_instructions: None,
                    reasoning: agentprism_ai::ReasoningLevel::Off,
                    timestamp: Timestamp::from_unix_millis(10),
                },
                CancellationToken::new(),
            )
            .await
            .expect("bash split compaction");
        assert_eq!(compacted.retained_tail, vec![assistant]);
        {
            let requests = runtime.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let history_prompt = message_text(&requests[0].context.messages[0]);
            let prefix_prompt = message_text(&requests[1].context.messages[0]);
            assert!(history_prompt.contains("[User]: old turn"));
            assert!(!prefix_prompt.contains("[User]: old turn"));
            assert!(prefix_prompt.contains("Ran `cargo test`"));
        }
    });
}

#[test]
fn compaction_records_tokens_before() {
    // Pi basis: packages/agent/test/harness/compaction.test.ts, `estimateContextTokens`
    // and preparation `tokensBefore` assertions; packages/agent/src/harness/messages.ts
    // supplies the known-role `convertToLlm` projection.
    block_on(async {
        let storage = storage();
        append_message(
            &storage,
            "main",
            "u1",
            None,
            user_record("u1", "12345678", 1),
        );
        context_policy(
            harness_session(storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::succeeding()),
        )
        .compact_manual(state_view(&agent_state()), None, CancellationToken::new())
        .await
        .unwrap();
        let entry = storage
            .state_snapshot()
            .unwrap()
            .entries_in_sequence_order()
            .into_iter()
            .find(|entry| matches!(entry, SessionEntry::Compaction { .. }))
            .cloned()
            .unwrap();
        assert!(matches!(
            entry,
            SessionEntry::Compaction {
                tokens_before: 2,
                ..
            }
        ));

        let branch_summary = AgentRecord::Custom {
            type_name: "branchSummary".to_owned(),
            payload: serde_json::value::to_raw_value(&serde_json::json!({
                "summary": "12345678",
                "fromId": "abandoned",
                "timestamp": 1
            }))
            .unwrap(),
        };
        let compaction_summary = AgentRecord::Custom {
            type_name: "compactionSummary".to_owned(),
            payload: serde_json::value::to_raw_value(&serde_json::json!({
                "summary": "abcdefgh",
                "tokensBefore": 99,
                "timestamp": 2
            }))
            .unwrap(),
        };
        let unknown = AgentRecord::Custom {
            type_name: "unknownAugmentedRole".to_owned(),
            payload: serde_json::value::to_raw_value(&serde_json::json!({
                "content": "must not reach the model",
                "timestamp": 3
            }))
            .unwrap(),
        };
        assert_eq!(estimate_record_tokens(&branch_summary).unwrap(), 2);
        assert_eq!(estimate_record_tokens(&compaction_summary).unwrap(), 2);
        assert_eq!(estimate_record_tokens(&unknown).unwrap(), 0);
        let projected =
            serialize_conversation(&[branch_summary.clone(), compaction_summary.clone(), unknown])
                .unwrap();
        assert!(projected.contains(BRANCH_SUMMARY_PREFIX));
        assert!(projected.contains(COMPACTION_SUMMARY_PREFIX));
        assert!(projected.contains("12345678"));
        assert!(projected.contains("abcdefgh"));
        assert!(!projected.contains("must not reach the model"));

        let structural_path = vec![
            SessionEntry::Compaction {
                base: EntryBase {
                    id: EntryId::new("structural-compaction"),
                    sequence: Sequence::new(1),
                    parent_id: None,
                    timestamp: Timestamp::from_unix_millis(1),
                },
                summary: "abcdefgh".to_owned(),
                retained_tail: Vec::new(),
                tokens_before: 99,
                details: None,
                usage: None,
            },
            SessionEntry::BranchSummary {
                base: EntryBase {
                    id: EntryId::new("structural-branch-summary"),
                    sequence: Sequence::new(2),
                    parent_id: Some(EntryId::new("structural-compaction")),
                    timestamp: Timestamp::from_unix_millis(2),
                },
                from_id: EntryId::new("abandoned"),
                summary: "12345678".to_owned(),
                details: None,
                usage: None,
            },
            SessionEntry::Message {
                base: EntryBase {
                    id: EntryId::new("structural-tail"),
                    sequence: Sequence::new(3),
                    parent_id: Some(EntryId::new("structural-branch-summary")),
                    timestamp: Timestamp::from_unix_millis(3),
                },
                message: user_record("structural-tail", "87654321", 3),
                terminate: false,
            },
        ];
        let structural_records = reconstruct_branch_context(&structural_path)
            .unwrap()
            .records;
        let structural_estimate = estimate_harness_context_tokens(&structural_records).unwrap();
        assert_eq!(structural_estimate.tokens, 6);
        assert_eq!(structural_estimate.trailing_tokens, 6);

        let mut reported = assistant_message(
            "reported",
            "ignored by provider usage",
            AssistantFinishReason::Stop,
            None,
            usage(30, 10),
        );
        reported.timestamp = Timestamp::from_unix_millis(1);
        let estimate = estimate_harness_context_tokens(&[
            user_record("before", "timestamp does not gate usage", 100),
            AgentRecord::Llm(Message::Assistant(reported.clone())),
            user_record("after", "12345678", 2),
        ])
        .unwrap();
        assert_eq!(estimate.tokens, 42);
        assert_eq!(estimate.usage_tokens, 40);
        assert_eq!(estimate.trailing_tokens, 2);
        assert_eq!(estimate.last_usage_index, Some(1));

        let compacted_path = vec![
            SessionEntry::Compaction {
                base: EntryBase {
                    id: EntryId::new("prior-compaction"),
                    sequence: Sequence::new(1),
                    parent_id: None,
                    timestamp: Timestamp::from_unix_millis(1),
                },
                summary: "a very long summary that must not be added after retained usage".into(),
                retained_tail: vec![AgentRecord::Llm(Message::Assistant(reported))],
                tokens_before: 999,
                details: None,
                usage: None,
            },
            SessionEntry::Message {
                base: EntryBase {
                    id: EntryId::new("retained-tail-message"),
                    sequence: Sequence::new(2),
                    parent_id: Some(EntryId::new("prior-compaction")),
                    timestamp: Timestamp::from_unix_millis(2),
                },
                message: user_record("retained-tail-message", "12345678", 2),
                terminate: false,
            },
        ];
        let reconstructed = reconstruct_branch_context(&compacted_path).unwrap();
        let retained_estimate = estimate_harness_context_tokens(&reconstructed.records).unwrap();
        assert_eq!(retained_estimate.tokens, 42);
        assert_eq!(retained_estimate.usage_tokens, 40);
        assert_eq!(retained_estimate.trailing_tokens, 2);
    });
}

#[test]
fn compaction_records_summary_usage() {
    // Pi basis: packages/agent/test/harness/compaction.test.ts, summary usage result assertions;
    // packages/agent/src/harness/compaction/compaction.ts `combineUsage` adds raw
    // `totalTokens` values instead of recalculating zero from component fields.
    block_on(async {
        let storage = storage();
        append_message(&storage, "main", "u1", None, user_record("u1", "old", 1));
        context_policy(
            harness_session(storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::succeeding()),
        )
        .compact_manual(state_view(&agent_state()), None, CancellationToken::new())
        .await
        .unwrap();
        let state = storage.state_snapshot().unwrap();
        assert!(state.records_in_sequence_order().iter().any(|record| {
            matches!(record, OperationRecord::Usage { attribution: UsageAttribution::Compaction { .. }, usage: observed, .. } if *observed == usage(7, 3))
        }));
        assert!(state.entries_in_sequence_order().iter().any(|entry| {
            matches!(entry, SessionEntry::Compaction { usage: Some(observed), .. } if *observed == usage(7, 3))
        }));

        let runtime = Arc::new(RecordingRuntime::new([agentprism_ai::text_response(
            "## Goal\nTest summary",
        )
        .with_usage(usage(11, 5))]));
        let policy = RuntimeCompactionPolicy::new(
            runtime.clone(),
            ModelRef::new("test", "summary"),
            100,
            128,
            true,
            CompactionSettings {
                enabled: true,
                reserve_tokens: 100,
                keep_recent_tokens: 20,
            },
        );
        let mut tool_message = assistant_message(
            "tool",
            "",
            AssistantFinishReason::ToolUse,
            None,
            usage(1, 1),
        );
        tool_message.content.push(ContentBlock::ToolCall {
            id: ContentBlockId::new("tool-block"),
            call: ToolCall {
                id: ToolCallId::new("call-1"),
                name: "read".to_owned(),
                arguments: serde_json::json!({"path": "src/index.ts"}),
            },
        });
        let records = vec![
            user_record("summary-user", "inspect", 1),
            AgentRecord::Custom {
                type_name: "custom".to_owned(),
                payload: serde_json::value::to_raw_value(&serde_json::json!({
                    "content": "custom context",
                    "timestamp": 2
                }))
                .unwrap(),
            },
            AgentRecord::Custom {
                type_name: "bashExecution".to_owned(),
                payload: serde_json::value::to_raw_value(&serde_json::json!({
                    "command": "cargo test",
                    "output": "ok",
                    "exitCode": 0,
                    "cancelled": false,
                    "truncated": false,
                    "timestamp": 3
                }))
                .unwrap(),
            },
            AgentRecord::Llm(Message::Assistant(tool_message)),
        ];
        assert_eq!(estimate_record_tokens(&records[1]).unwrap(), 4);
        assert_eq!(estimate_record_tokens(&records[2]).unwrap(), 3);
        let result = policy
            .compact(
                CompactionInput {
                    retained_tail_start: records.len(),
                    records,
                    structural_branch_summary_indices: Vec::new(),
                    tokens_before: 99,
                    reason: CompactionReason::Manual,
                    summary_model: ModelRef::new("test", "summary"),
                    result_entry_id: EntryId::new("summary-entry"),
                    previous_summary: Some("old summary".to_owned()),
                    previous_details: None,
                    custom_instructions: Some("focus".to_owned()),
                    reasoning: agentprism_ai::ReasoningLevel::Medium,
                    timestamp: Timestamp::from_unix_millis(50),
                },
                CancellationToken::new(),
            )
            .await
            .expect("runtime compaction");
        assert_eq!(result.usage, Some(usage(11, 5)));
        assert!(
            result
                .summary
                .contains("<read-files>\nsrc/index.ts\n</read-files>")
        );
        assert_eq!(
            result.details.as_ref().unwrap().value.get(),
            r#"{"readFiles":["src/index.ts"],"modifiedFiles":[]}"#
        );
        {
            let requests = runtime.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].options.max_output_tokens, Some(80));
            assert_eq!(
                requests[0].options.reasoning,
                Some(agentprism_ai::ReasoningLevel::Medium)
            );
            assert_eq!(
                requests[0].options.cache_retention,
                Some(CacheRetention::None)
            );
            assert!(requests[0].options.session_id.is_some());
            let prompt = match &requests[0].context.messages[0] {
                Message::User(message) => match &message.content[0] {
                    ContentBlock::Text { text, .. } => text,
                    _ => panic!("summary prompt must be text"),
                },
                _ => panic!("summary request must contain one user message"),
            };
            assert!(prompt.contains("<previous-summary>\nold summary\n</previous-summary>"));
            assert!(prompt.contains("Additional focus: focus"));
            assert!(prompt.contains("[User]: custom context"));
            assert!(prompt.contains("[User]: Ran `cargo test`\n```\nok\n```"));
            assert!(prompt.contains(r#"read(path="src/index.ts")"#));
        }

        let empty_runtime = Arc::new(RecordingRuntime::new([agentprism_ai::text_response(
            "empty option summary",
        )]));
        let empty_policy = RuntimeCompactionPolicy::new(
            empty_runtime.clone(),
            ModelRef::new("test", "summary"),
            100,
            128,
            false,
            CompactionSettings {
                enabled: true,
                reserve_tokens: 100,
                keep_recent_tokens: 20,
            },
        );
        empty_policy
            .compact(
                CompactionInput {
                    records: vec![user_record("empty-options", "summarize", 1)],
                    structural_branch_summary_indices: Vec::new(),
                    retained_tail_start: 1,
                    tokens_before: 2,
                    reason: CompactionReason::Manual,
                    summary_model: ModelRef::new("test", "summary"),
                    result_entry_id: EntryId::new("empty-options-summary"),
                    previous_summary: Some(String::new()),
                    previous_details: None,
                    custom_instructions: Some(String::new()),
                    reasoning: agentprism_ai::ReasoningLevel::Off,
                    timestamp: Timestamp::from_unix_millis(51),
                },
                CancellationToken::new(),
            )
            .await
            .expect("empty summary options use the initial Pi prompt");
        {
            let empty_requests = empty_runtime.requests.lock().unwrap();
            let empty_prompt = message_text(&empty_requests[0].context.messages[0]);
            assert!(empty_prompt.contains("The messages above are a conversation to summarize."));
            assert!(!empty_prompt.contains("NEW conversation messages"));
            assert!(!empty_prompt.contains("<previous-summary>"));
            assert!(!empty_prompt.contains("Additional focus:"));
        }

        let mut zero_total = usage(20, 5);
        zero_total.total_tokens = Some(0);
        let mut reported_total = usage(3, 2);
        reported_total.total_tokens = Some(7);
        let split_runtime = Arc::new(RecordingRuntime::new([
            agentprism_ai::text_response("history summary").with_usage(zero_total),
            agentprism_ai::text_response("turn prefix summary").with_usage(reported_total),
        ]));
        let split_policy = RuntimeCompactionPolicy::new(
            split_runtime,
            ModelRef::new("test", "summary"),
            100_000,
            2_048,
            false,
            CompactionSettings::default(),
        );
        let retained = AgentRecord::Llm(Message::Assistant(assistant_message(
            "retained",
            "retained suffix",
            AssistantFinishReason::Stop,
            None,
            usage(1, 1),
        )));
        let split_result = split_policy
            .compact(
                CompactionInput {
                    records: vec![
                        user_record("history", "old history", 1),
                        user_record("turn", "large current turn", 2),
                        retained,
                    ],
                    structural_branch_summary_indices: Vec::new(),
                    retained_tail_start: 2,
                    tokens_before: 99,
                    reason: CompactionReason::Manual,
                    summary_model: ModelRef::new("test", "summary"),
                    result_entry_id: EntryId::new("raw-total-split"),
                    previous_summary: None,
                    previous_details: None,
                    custom_instructions: None,
                    reasoning: agentprism_ai::ReasoningLevel::Off,
                    timestamp: Timestamp::from_unix_millis(60),
                },
                CancellationToken::new(),
            )
            .await
            .expect("split summary usage aggregation");
        let combined = split_result.usage.expect("split summaries report usage");
        assert_eq!(combined.input_tokens, 23);
        assert_eq!(combined.output_tokens, 7);
        assert_eq!(combined.total_tokens, Some(7));
    });
}

#[test]
fn compaction_failure_does_not_move_branch_head() {
    // Pi basis: packages/agent/test/harness/compaction.test.ts, error-result behavior; architecture §7.7 atomic head rule.
    block_on(async {
        let storage = storage();
        append_message(&storage, "main", "u1", None, user_record("u1", "old", 1));
        let result = context_policy(
            harness_session(storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::fail_once()),
        )
        .compact_manual(state_view(&agent_state()), None, CancellationToken::new())
        .await;
        assert!(result.is_err());
        assert_eq!(
            storage
                .state_snapshot()
                .unwrap()
                .lane_leaf(&LaneName::new("main")),
            Some(&Some(EntryId::new("u1")))
        );
    });
}

#[test]
fn compaction_operation_can_resume() {
    // Pi basis: packages/agent/test/harness/reducer.test.ts, "compaction retry",
    // hook-supplied completed targets, and validateAttemptSequence.
    block_on(async {
        let primary_storage = storage();
        append_message(
            &primary_storage,
            "main",
            "u1",
            None,
            user_record("u1", "old", 1),
        );
        let harness = context_policy(
            harness_session(primary_storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::fail_once()),
        );
        assert!(
            harness
                .compact_manual(
                    state_view(&agent_state()),
                    Some("focus".into()),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert!(matches!(
            primary_storage
                .state_snapshot()
                .unwrap()
                .recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Resume { .. }
        ));
        harness
            .resume_compaction(state_view(&agent_state()), CancellationToken::new())
            .await
            .expect("resumed compaction");
        let state = primary_storage.state_snapshot().unwrap();
        assert!(matches!(
            state.recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Idle
        ));
        assert!(
            state
                .entries_in_sequence_order()
                .iter()
                .any(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        );

        let committed_storage = storage();
        append_message(
            &committed_storage,
            "main",
            "committed-user",
            None,
            user_record("committed-user", "old", 1),
        );
        append_operation_started(
            &committed_storage,
            "main",
            "committed-operation",
            Some("committed-user"),
            OperationIntent::Compaction {
                custom_instructions: Some("recorded focus".to_owned()),
                result_entry_id: EntryId::new("committed-compaction"),
            },
        );
        let committed_state = committed_storage.state_snapshot().unwrap();
        let attempt_sequence = committed_state.next_sequence().unwrap();
        let entry_sequence = Sequence::new(attempt_sequence.get() + 1);
        committed_storage
            .append_batch(
                committed_state.sequence(),
                vec![
                    SessionMutation::Record {
                        record: OperationRecord::StepAttempt {
                            base: operation_base("committed-attempt", "main", attempt_sequence),
                            run_id: RunId::new("committed-operation"),
                            step: OperationStep::Compaction,
                            attempt: 1,
                            result_entry_id: EntryId::new("committed-compaction"),
                            compaction_reason: Some(CompactionReason::Manual),
                        },
                    },
                    SessionMutation::Entry {
                        lane: Some(LaneName::new("main")),
                        entry: SessionEntry::Compaction {
                            base: EntryBase {
                                id: EntryId::new("committed-compaction"),
                                sequence: entry_sequence,
                                parent_id: Some(EntryId::new("committed-user")),
                                timestamp: Timestamp::from_unix_millis(3),
                            },
                            summary: "already committed".to_owned(),
                            retained_tail: Vec::new(),
                            tokens_before: 20,
                            details: None,
                            usage: None,
                        },
                    },
                ],
            )
            .unwrap();
        context_policy(
            harness_session(committed_storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::fail_once()),
        )
        .resume_compaction(state_view(&agent_state()), CancellationToken::new())
        .await
        .expect("committed compaction target closes without regeneration");
        assert!(matches!(
            committed_storage
                .state_snapshot()
                .unwrap()
                .recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Idle
        ));

        let series_storage = storage();
        append_message(
            &series_storage,
            "main",
            "series-user",
            None,
            user_record("series-user", "old", 1),
        );
        append_operation_started(
            &series_storage,
            "main",
            "series-run",
            Some("series-user"),
            OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: Default::default(),
            },
        );
        let series_state = series_storage.state_snapshot().unwrap();
        let old_attempt_sequence = series_state.next_sequence().unwrap();
        let old_entry_sequence = Sequence::new(old_attempt_sequence.get() + 1);
        series_storage
            .append_batch(
                series_state.sequence(),
                vec![
                    SessionMutation::Record {
                        record: OperationRecord::StepAttempt {
                            base: operation_base(
                                "series-old-attempt",
                                "main",
                                old_attempt_sequence,
                            ),
                            run_id: RunId::new("series-run"),
                            step: OperationStep::Compaction,
                            attempt: 1,
                            result_entry_id: EntryId::new("series-old-compaction"),
                            compaction_reason: Some(CompactionReason::Threshold),
                        },
                    },
                    SessionMutation::Entry {
                        lane: Some(LaneName::new("main")),
                        entry: SessionEntry::Compaction {
                            base: EntryBase {
                                id: EntryId::new("series-old-compaction"),
                                sequence: old_entry_sequence,
                                parent_id: Some(EntryId::new("series-user")),
                                timestamp: Timestamp::from_unix_millis(3),
                            },
                            summary: "old summary".to_owned(),
                            retained_tail: Vec::new(),
                            tokens_before: 20,
                            details: None,
                            usage: None,
                        },
                    },
                ],
            )
            .unwrap();
        append_message(
            &series_storage,
            "main",
            "series-tail",
            Some("series-old-compaction"),
            user_record("series-tail", "new input", 4),
        );
        context_policy(
            harness_session(series_storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::succeeding()),
        )
        .prepare_agent_records(state_view(&agent_state()), CancellationToken::new())
        .await
        .expect("new completed-step series compacts");
        let attempts = series_storage
            .state_snapshot()
            .unwrap()
            .records_in_sequence_order()
            .iter()
            .filter_map(|record| match record {
                OperationRecord::StepAttempt {
                    run_id,
                    step: OperationStep::Compaction,
                    attempt,
                    ..
                } if *run_id == RunId::new("series-run") => Some(*attempt),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(attempts, vec![1, 1]);
    });
}

#[test]
fn compaction_context_uses_latest_compaction_entry() {
    // Pi basis: packages/agent/test/harness/session/context.test.ts, latest compaction boundary.
    let old = user_record("old", "old", 1);
    let retained = user_record("retained", "retained", 2);
    let tail = user_record("tail", "tail", 4);
    let entries = vec![
        SessionEntry::Message {
            base: EntryBase {
                id: EntryId::new("old"),
                sequence: Sequence::new(1),
                parent_id: None,
                timestamp: Timestamp::from_unix_millis(1),
            },
            message: old,
            terminate: false,
        },
        SessionEntry::Compaction {
            base: EntryBase {
                id: EntryId::new("compact"),
                sequence: Sequence::new(2),
                parent_id: Some(EntryId::new("old")),
                timestamp: Timestamp::from_unix_millis(2),
            },
            summary: "latest summary".into(),
            retained_tail: vec![retained],
            tokens_before: 100,
            details: None,
            usage: None,
        },
        SessionEntry::Message {
            base: EntryBase {
                id: EntryId::new("tail"),
                sequence: Sequence::new(3),
                parent_id: Some(EntryId::new("compact")),
                timestamp: Timestamp::from_unix_millis(3),
            },
            message: tail,
            terminate: false,
        },
    ];
    let context = reconstruct_branch_context(&entries).unwrap();
    assert_eq!(context.records.len(), 3);
    let rendered = serialize_conversation(&context.records).unwrap();
    assert!(rendered.contains("latest summary"));
    assert!(rendered.contains("retained"));
    assert!(rendered.contains("tail"));
    assert!(!rendered.contains("[User]: old"));

    block_on(async {
        let custom_storage = storage();
        append_message(
            &custom_storage,
            "main",
            "custom-user",
            None,
            user_record("custom-user", "visible", 1),
        );
        append_custom_entry(
            &custom_storage,
            "main",
            "custom-entry",
            Some("custom-user"),
            "unknown",
            serde_json::json!({"secret": "must-not-leak"}),
        );
        let omitted = context_policy(
            harness_session(custom_storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::only_forced()),
        )
        .prepare_agent_records(state_view(&agent_state()), CancellationToken::new())
        .await
        .expect("unknown custom entries are omitted");
        assert_eq!(omitted.records.len(), 1);
        assert!(
            !serialize_conversation(&omitted.records)
                .unwrap()
                .contains("must-not-leak")
        );

        let mut explicit = context_policy(
            harness_session(custom_storage.clone(), "main"),
            Arc::new(FixedCompactionPolicy::only_forced()),
        );
        explicit.custom_entry_projector = Arc::new(ExplicitCustomProjector);
        let projected = explicit
            .prepare_agent_records(state_view(&agent_state()), CancellationToken::new())
            .await
            .expect("registered custom entry projector");
        assert_eq!(projected.records.len(), 2);
        assert!(
            serialize_conversation(&projected.records)
                .unwrap()
                .contains("must-not-leak")
        );

        let transformed_storage = storage();
        append_message(
            &transformed_storage,
            "main",
            "transform-old",
            None,
            user_record("transform-old", "old", 1),
        );
        append_compaction(
            &transformed_storage,
            "main",
            "transform-compaction",
            Some("transform-old"),
        );
        append_custom_entry(
            &transformed_storage,
            "main",
            "transform-custom",
            Some("transform-compaction"),
            "note",
            serde_json::json!({"content": "projected"}),
        );
        let observing = ObservingCustomProjector::default();
        let observations = observing.observations.clone();
        let mut transformed = context_policy(
            harness_session(transformed_storage, "main"),
            Arc::new(FixedCompactionPolicy::only_forced()),
        );
        transformed.custom_entry_projector = Arc::new(observing);
        transformed
            .prepare_agent_records(state_view(&agent_state()), CancellationToken::new())
            .await
            .expect("projector receives transformed Send context path");
        assert_eq!(
            *observations.lock().unwrap(),
            vec![(
                1,
                vec![
                    "transform-compaction".to_owned(),
                    "transform-custom".to_owned()
                ]
            )]
        );

        let local_transformed_storage = local_storage();
        append_message(
            &local_transformed_storage,
            "main",
            "local-transform-old",
            None,
            user_record("local-transform-old", "old", 1),
        );
        append_compaction(
            &local_transformed_storage,
            "main",
            "local-transform-compaction",
            Some("local-transform-old"),
        );
        append_custom_entry(
            &local_transformed_storage,
            "main",
            "local-transform-custom",
            Some("local-transform-compaction"),
            "note",
            serde_json::json!({"content": "projected"}),
        );
        let local_observing = ObservingCustomProjector::default();
        let local_observations = local_observing.observations.clone();
        let mut local_transformed = local_context_policy(
            local_harness_session(local_transformed_storage, "main"),
            Rc::new(FixedCompactionPolicy::only_forced()),
        );
        local_transformed.custom_entry_projector = Rc::new(local_observing);
        LocalContextPolicy::prepare_agent_records(
            &local_transformed,
            state_view(&agent_state()),
            CancellationToken::new(),
        )
        .await
        .expect("projector receives transformed Local context path");
        assert_eq!(
            *local_observations.lock().unwrap(),
            vec![(
                1,
                vec![
                    "local-transform-compaction".to_owned(),
                    "local-transform-custom".to_owned()
                ]
            )]
        );

        let summary_custom_storage = storage();
        append_message(
            &summary_custom_storage,
            "main",
            "summary-custom-user",
            None,
            user_record("summary-custom-user", "visible history", 1),
        );
        append_custom_entry(
            &summary_custom_storage,
            "main",
            "summary-custom-entry",
            Some("summary-custom-user"),
            "note",
            serde_json::json!({"secret": "summary-projector-must-not-run"}),
        );
        append_message(
            &summary_custom_storage,
            "main",
            "summary-custom-tail",
            Some("summary-custom-entry"),
            user_record("summary-custom-tail", "tail", 3),
        );
        let summary_runtime = Arc::new(RecordingRuntime::new([agentprism_ai::text_response(
            "custom omission summary",
        )]));
        let summary_policy = Arc::new(RuntimeCompactionPolicy::new(
            summary_runtime.clone(),
            ModelRef::new("test", "summary"),
            100_000,
            2_048,
            false,
            CompactionSettings {
                keep_recent_tokens: 1,
                ..CompactionSettings::default()
            },
        ));
        let mut summary_context = context_policy(
            harness_session(summary_custom_storage.clone(), "main"),
            summary_policy,
        );
        let summary_observing = ObservingCustomProjector::default();
        let summary_observations = summary_observing.observations.clone();
        summary_context.custom_entry_projector = Arc::new(summary_observing);
        summary_context
            .compact_manual(state_view(&agent_state()), None, CancellationToken::new())
            .await
            .expect("custom session entry is omitted from compaction preparation");
        assert!(summary_observations.lock().unwrap().is_empty());
        {
            let requests = summary_runtime.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let prompt = message_text(&requests[0].context.messages[0]);
            assert!(prompt.contains("visible history"));
            assert!(!prompt.contains("summary-projector-must-not-run"));
        }
        let expected_tokens = estimate_harness_context_tokens(&[
            user_record("summary-custom-user", "visible history", 1),
            user_record("summary-custom-tail", "tail", 3),
        ])
        .unwrap()
        .tokens;
        assert!(summary_custom_storage
            .state_snapshot()
            .unwrap()
            .entries_in_sequence_order()
            .iter()
            .any(|entry| matches!(entry, SessionEntry::Compaction { tokens_before, .. } if *tokens_before == expected_tokens)));

        let configured = SimpleGenerationOptions {
            max_retries: Some(3),
            max_output_tokens: Some(777),
            temperature: Some(0.25),
            stop: vec!["done".to_owned()],
            seed: Some(42),
            reasoning: Some(agentprism_ai::ReasoningLevel::Medium),
            ..SimpleGenerationOptions::default()
        };

        let high_storage = storage();
        append_message(
            &high_storage,
            "main",
            "high-user",
            None,
            user_record("high-user", "high", 1),
        );
        append_reasoning_change(
            &high_storage,
            "main",
            "high-reasoning",
            Some("high-user"),
            agentprism_ai::ReasoningLevel::High,
        );
        let default_agent_state = agent_state();
        let high = HarnessContextPolicy {
            base: Arc::new(DefaultContextPolicy),
            compaction: Arc::new(FixedCompactionPolicy::only_forced()),
            session: harness_session(high_storage, "main"),
            custom_entry_projector: Arc::new(OmitCustomSessionEntries),
        }
        .prepare_agent_records(
            state_view_with_options(&default_agent_state, &configured),
            CancellationToken::new(),
        )
        .await
        .expect("durable high reasoning override");
        let high_options = high.options_override.expect("complete options override");
        assert_eq!(
            high_options.reasoning,
            Some(agentprism_ai::ReasoningLevel::High)
        );
        assert_eq!(high_options.max_retries, Some(3));
        assert_eq!(high_options.max_output_tokens, Some(777));
        assert_eq!(high_options.temperature, Some(0.25));
        assert_eq!(high_options.stop, vec!["done"]);
        assert_eq!(high_options.seed, Some(42));

        let off_storage = storage();
        append_message(
            &off_storage,
            "main",
            "off-user",
            None,
            user_record("off-user", "off", 1),
        );
        append_reasoning_change(
            &off_storage,
            "main",
            "off-reasoning",
            Some("off-user"),
            agentprism_ai::ReasoningLevel::Off,
        );
        let non_off_state = AgentState::new(
            "system",
            ModelRef::new("test", "model"),
            agentprism_ai::ReasoningLevel::High,
        );
        let off = HarnessContextPolicy {
            base: Arc::new(DefaultContextPolicy),
            compaction: Arc::new(FixedCompactionPolicy::only_forced()),
            session: harness_session(off_storage, "main"),
            custom_entry_projector: Arc::new(OmitCustomSessionEntries),
        }
        .prepare_agent_records(
            state_view_with_options(&non_off_state, &configured),
            CancellationToken::new(),
        )
        .await
        .expect("durable off reasoning override");
        let off_options = off.options_override.expect("explicit off override");
        assert_eq!(off_options.reasoning, None);
        assert_eq!(off_options.max_retries, Some(3));
        assert_eq!(off_options.max_output_tokens, Some(777));
        assert_eq!(off_options.temperature, Some(0.25));
        assert_eq!(off_options.stop, vec!["done"]);
        assert_eq!(off_options.seed, Some(42));

        let local_default_storage = local_storage();
        append_message(
            &local_default_storage,
            "main",
            "local-default-user",
            None,
            user_record("local-default-user", "local high", 1),
        );
        append_reasoning_change(
            &local_default_storage,
            "main",
            "local-default-reasoning",
            Some("local-default-user"),
            agentprism_ai::ReasoningLevel::High,
        );
        let local_default_state = agent_state();
        let local_default = LocalContextPolicy::prepare_agent_records(
            &local_context_policy(
                local_harness_session(local_default_storage, "main"),
                Rc::new(FixedCompactionPolicy::only_forced()),
            ),
            state_view_with_options(&local_default_state, &configured),
            CancellationToken::new(),
        )
        .await
        .expect("local durable reasoning override");
        let local_default_options = local_default
            .options_override
            .expect("local complete options override");
        assert_eq!(
            local_default_options.reasoning,
            Some(agentprism_ai::ReasoningLevel::High)
        );
        assert_eq!(local_default_options.max_output_tokens, Some(777));
        assert_eq!(local_default_options.seed, Some(42));

        let send_authoritative_storage = storage();
        append_message(
            &send_authoritative_storage,
            "main",
            "send-authoritative-user",
            None,
            user_record("send-authoritative-user", "send authority", 1),
        );
        append_reasoning_change(
            &send_authoritative_storage,
            "main",
            "send-authoritative-reasoning",
            Some("send-authoritative-user"),
            agentprism_ai::ReasoningLevel::High,
        );
        let send_base = AuthoritativeOptionsContextPolicy::default();
        let send_observations = send_base.observations.clone();
        let send_authoritative = HarnessContextPolicy {
            base: Arc::new(send_base),
            compaction: Arc::new(FixedCompactionPolicy::only_forced()),
            session: harness_session(send_authoritative_storage, "main"),
            custom_entry_projector: Arc::new(OmitCustomSessionEntries),
        }
        .prepare_agent_records(
            state_view_with_options(&default_agent_state, &configured),
            CancellationToken::new(),
        )
        .await
        .expect("send base policy remains final options authority");
        assert_eq!(
            *send_observations.lock().unwrap(),
            vec![(
                agentprism_ai::ReasoningLevel::High,
                Some(agentprism_ai::ReasoningLevel::High)
            )]
        );
        let send_authoritative_options = send_authoritative
            .options_override
            .expect("send base policy supplied an override");
        assert_eq!(
            send_authoritative_options.reasoning,
            Some(agentprism_ai::ReasoningLevel::Low)
        );
        assert_eq!(send_authoritative_options.max_output_tokens, Some(777));
        assert_eq!(send_authoritative_options.seed, Some(9_001));

        let local_authoritative_storage = local_storage();
        append_message(
            &local_authoritative_storage,
            "main",
            "local-authoritative-user",
            None,
            user_record("local-authoritative-user", "local authority", 1),
        );
        append_reasoning_change(
            &local_authoritative_storage,
            "main",
            "local-authoritative-reasoning",
            Some("local-authoritative-user"),
            agentprism_ai::ReasoningLevel::High,
        );
        let local_base = AuthoritativeOptionsContextPolicy::default();
        let local_observations = local_base.observations.clone();
        let local_authoritative = LocalContextPolicy::prepare_agent_records(
            &LocalHarnessContextPolicy {
                base: Rc::new(local_base),
                compaction: Rc::new(FixedCompactionPolicy::only_forced()),
                session: local_harness_session(local_authoritative_storage, "main"),
                custom_entry_projector: Rc::new(OmitCustomSessionEntries),
            },
            state_view_with_options(&default_agent_state, &configured),
            CancellationToken::new(),
        )
        .await
        .expect("local base policy remains final options authority");
        assert_eq!(
            *local_observations.lock().unwrap(),
            vec![(
                agentprism_ai::ReasoningLevel::High,
                Some(agentprism_ai::ReasoningLevel::High)
            )]
        );
        let local_authoritative_options = local_authoritative
            .options_override
            .expect("local base policy supplied an override");
        assert_eq!(
            local_authoritative_options.reasoning,
            Some(agentprism_ai::ReasoningLevel::Low)
        );
        assert_eq!(local_authoritative_options.max_output_tokens, Some(777));
        assert_eq!(local_authoritative_options.seed, Some(9_001));
    });
}

#[test]
fn compaction_context_uses_requested_model_after_response_echo() {
    // Pi basis: packages/agent/src/harness/session/context.ts derives active state
    // from AssistantMessage.model, which is the requested model in the Rust split.
    let mut assistant = assistant_message(
        "model-echo-assistant",
        "answer",
        AssistantFinishReason::Stop,
        None,
        usage(4, 1),
    );
    assistant.requested_model = ModelId::new("requested-model");
    assistant.response_model = Some(ModelId::new("concrete-response-model"));
    let expected = ModelRef::new("test", "requested-model");
    let entry = SessionEntry::Message {
        base: EntryBase {
            id: EntryId::new("model-echo-entry"),
            sequence: Sequence::new(1),
            parent_id: None,
            timestamp: Timestamp::from_unix_millis(1),
        },
        message: AgentRecord::Llm(Message::Assistant(assistant.clone())),
        terminate: false,
    };
    assert_eq!(
        reconstruct_branch_context(std::slice::from_ref(&entry))
            .unwrap()
            .model,
        Some(expected.clone())
    );

    block_on(async {
        let send_storage = storage();
        append_message(
            &send_storage,
            "main",
            "send-model-echo-entry",
            None,
            AgentRecord::Llm(Message::Assistant(assistant.clone())),
        );
        let prepared = context_policy(
            harness_session(send_storage, "main"),
            Arc::new(FixedCompactionPolicy::only_forced()),
        )
        .prepare_agent_records(state_view(&agent_state()), CancellationToken::new())
        .await
        .expect("send preparation with a response-model echo");
        assert_eq!(prepared.model_override, Some(expected.clone()));

        let local_storage = local_storage();
        append_message(
            &local_storage,
            "main",
            "local-model-echo-entry",
            None,
            AgentRecord::Llm(Message::Assistant(assistant)),
        );
        let prepared = agentprism_core::LocalContextPolicy::prepare_agent_records(
            &local_context_policy(
                local_harness_session(local_storage, "main"),
                Rc::new(FixedCompactionPolicy::only_forced()),
            ),
            state_view(&agent_state()),
            CancellationToken::new(),
        )
        .await
        .expect("local preparation with a response-model echo");
        assert_eq!(prepared.model_override, Some(expected));
    });
}

struct RecordingBranchPolicy {
    fail_count: Mutex<usize>,
    abandoned: Mutex<Vec<Vec<EntryId>>>,
    active_models: Mutex<Vec<Option<ModelRef>>>,
}

impl RecordingBranchPolicy {
    fn new(fail_count: usize) -> Self {
        Self {
            fail_count: Mutex::new(fail_count),
            abandoned: Mutex::new(Vec::new()),
            active_models: Mutex::new(Vec::new()),
        }
    }
}

impl BranchSummaryPolicy for RecordingBranchPolicy {
    fn summarize(
        &self,
        input: BranchSummaryInput,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<BranchSummaryResult, BranchSummaryError>> {
        Box::pin(async move {
            self.active_models
                .lock()
                .unwrap()
                .push(input.active_model.clone());
            self.abandoned.lock().unwrap().push(
                input
                    .abandoned_entries
                    .iter()
                    .map(|entry| entry.id().clone())
                    .collect(),
            );
            let mut failures = self.fail_count.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(BranchSummaryError::summarization("scripted branch failure"));
            }
            Ok(BranchSummaryResult {
                summary: "branch summary".into(),
                details: None,
                usage: Some(usage(4, 2)),
                cost: None,
                stop_reason: AssistantFinishReason::Stop,
            })
        })
    }
}

struct LocalRecordingBranchPolicy {
    fail_count: RefCell<usize>,
    abandoned: RefCell<Vec<Vec<EntryId>>>,
}

impl LocalRecordingBranchPolicy {
    fn new(fail_count: usize) -> Self {
        Self {
            fail_count: RefCell::new(fail_count),
            abandoned: RefCell::new(Vec::new()),
        }
    }
}

impl LocalBranchSummaryPolicy for LocalRecordingBranchPolicy {
    fn summarize(
        &self,
        input: BranchSummaryInput,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<BranchSummaryResult, BranchSummaryError>> {
        Box::pin(async move {
            self.abandoned.borrow_mut().push(
                input
                    .abandoned_entries
                    .iter()
                    .map(|entry| entry.id().clone())
                    .collect(),
            );
            let mut failures = self.fail_count.borrow_mut();
            if *failures > 0 {
                *failures -= 1;
                return Err(BranchSummaryError::summarization(
                    "scripted local branch failure",
                ));
            }
            Ok(BranchSummaryResult {
                summary: "local branch summary".into(),
                details: None,
                usage: Some(usage(4, 2)),
                cost: None,
                stop_reason: AssistantFinishReason::Stop,
            })
        })
    }
}

fn branched_storage() -> Arc<InMemorySessionStorage> {
    let storage = storage();
    append_message(
        &storage,
        "main",
        "root",
        None,
        user_record("root", "root", 1),
    );
    append_message(
        &storage,
        "main",
        "common",
        Some("root"),
        user_record("common", "common", 2),
    );
    append_message(
        &storage,
        "main",
        "abandoned-1",
        Some("common"),
        user_record("a1", "abandoned one", 3),
    );
    append_message(
        &storage,
        "main",
        "abandoned-2",
        Some("abandoned-1"),
        user_record("a2", "abandoned two", 4),
    );
    create_lane(&storage, "target", "common");
    append_message(
        &storage,
        "target",
        "target-1",
        Some("common"),
        user_record("target", "target", 5),
    );
    storage
}

fn rc_only_branched_storage() -> Rc<RcOnlyStorage> {
    let storage = Rc::new(RcOnlyStorage::new());
    append_message(
        &storage.inner,
        "main",
        "local-root",
        None,
        user_record("local-root", "root", 1),
    );
    append_message(
        &storage.inner,
        "main",
        "local-abandoned",
        Some("local-root"),
        user_record("local-abandoned", "abandoned", 2),
    );
    storage
}

fn navigator(
    storage: Arc<InMemorySessionStorage>,
    policy: Arc<dyn BranchSummaryPolicy>,
) -> BranchNavigator {
    BranchNavigator {
        session: harness_session(storage, "main"),
        policy,
        summary_model: ModelRef::new("test", "summary"),
        token_budget: 1_000,
    }
}

fn local_navigator(
    storage: Rc<RcOnlyStorage>,
    policy: Rc<dyn LocalBranchSummaryPolicy>,
) -> LocalBranchNavigator {
    LocalBranchNavigator {
        session: rc_only_harness_session(storage, "main"),
        policy,
        summary_model: ModelRef::new("test", "summary"),
        token_budget: 1_000,
    }
}

#[test]
fn branch_summary_finds_common_ancestor() {
    // Pi basis: packages/agent/test/harness/branch-summarization.test.ts.
    let state = branched_storage().state_snapshot().unwrap();
    let collected = collect_entries_for_branch_summary(
        &state,
        Some(&EntryId::new("abandoned-2")),
        Some(&EntryId::new("target-1")),
    )
    .unwrap();
    assert_eq!(collected.common_ancestor_id, Some(EntryId::new("common")));
    let no_previous =
        collect_entries_for_branch_summary(&state, None, Some(&EntryId::new("target-1"))).unwrap();
    assert!(no_previous.entries.is_empty());
    assert_eq!(no_previous.common_ancestor_id, None);
}

#[test]
fn branch_summary_uses_requested_model_after_response_echo() {
    // Pi basis: packages/agent/src/harness/session/context.ts derives active state
    // from AssistantMessage.model, not provider response metadata.
    block_on(async {
        let storage = branched_storage();
        let mut assistant = assistant_message(
            "branch-model-echo",
            "answer",
            AssistantFinishReason::Stop,
            None,
            usage(4, 1),
        );
        assistant.requested_model = ModelId::new("requested-branch-model");
        assistant.response_model = Some(ModelId::new("concrete-branch-response-model"));
        append_message(
            &storage,
            "main",
            "branch-model-echo-entry",
            Some("abandoned-2"),
            AgentRecord::Llm(Message::Assistant(assistant)),
        );
        let policy = Arc::new(RecordingBranchPolicy::new(0));
        navigator(storage, policy.clone())
            .navigate(
                Some(EntryId::new("target-1")),
                true,
                None,
                CancellationToken::new(),
            )
            .await
            .expect("summarized navigation with response-model metadata");
        assert_eq!(
            policy.active_models.lock().unwrap().as_slice(),
            &[Some(ModelRef::new("test", "requested-branch-model"))]
        );
    });
}

#[test]
fn branch_summary_zero_context_window_uses_pi_fallback_budget() {
    // Pi basis: packages/agent/src/harness/compaction/branch-summarization.ts uses
    // model.contextWindow || 128000 before subtracting the 16,384-token reserve.
    let send = RuntimeBranchSummaryPolicy::new(
        Arc::new(agentprism_ai::ScriptedRuntime::new([])),
        ModelRef::new("test", "summary"),
        0,
    );
    let local = LocalRuntimeBranchSummaryPolicy::new(
        Rc::new(agentprism_ai::ScriptedRuntime::new([])),
        ModelRef::new("test", "summary"),
        0,
    );
    assert_eq!(send.token_budget(), 128_000 - 16_384);
    assert_eq!(local.token_budget(), 128_000 - 16_384);
}

#[test]
fn branch_summary_summarizes_abandoned_segment() {
    // Pi basis: packages/agent/test/harness/branch-summarization.test.ts chronological abandoned
    // side and packages/agent/src/harness/compaction/branch-summarization.ts
    // `prepareBranchEntries`, which estimates summary roles from `summary` before wrapping them.
    block_on(async {
        let storage = branched_storage();
        let policy = Arc::new(RecordingBranchPolicy::new(0));
        navigator(storage, policy.clone())
            .navigate(
                Some(EntryId::new("target-1")),
                true,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            policy.abandoned.lock().unwrap().as_slice(),
            &[vec![
                EntryId::new("abandoned-1"),
                EntryId::new("abandoned-2")
            ]]
        );

        let runtime = Arc::new(RecordingRuntime::new([agentprism_ai::text_response(
            "## Goal\nBranch work",
        )
        .with_usage(usage(8, 4))]));
        let runtime_policy = RuntimeBranchSummaryPolicy::new(
            runtime.clone(),
            ModelRef::new("test", "summary"),
            128_000,
        );
        let mut tool_message = assistant_message(
            "branch-tool",
            "",
            AssistantFinishReason::ToolUse,
            None,
            usage(2, 1),
        );
        tool_message.content.push(ContentBlock::ToolCall {
            id: ContentBlockId::new("branch-tool-block"),
            call: ToolCall {
                id: ToolCallId::new("branch-call"),
                name: "write".to_owned(),
                arguments: serde_json::json!({"path": "src/branch.rs"}),
            },
        });
        let result = runtime_policy
            .summarize(
                BranchSummaryInput {
                    common_ancestor_id: None,
                    abandoned_entries: vec![SessionEntry::Message {
                        base: EntryBase {
                            id: EntryId::new("branch-tool-entry"),
                            sequence: Sequence::new(1),
                            parent_id: None,
                            timestamp: Timestamp::from_unix_millis(1),
                        },
                        message: AgentRecord::Llm(Message::Assistant(tool_message)),
                        terminate: false,
                    }],
                    target_tail: Vec::new(),
                    custom_instructions: Some("preserve decisions".to_owned()),
                    replace_instructions: false,
                    active_model: None,
                    reasoning: agentprism_ai::ReasoningLevel::Off,
                    active_tool_names: vec!["write".to_owned()],
                    token_budget: 100_000,
                    summary_model: ModelRef::new("test", "summary"),
                    result_entry_id: EntryId::new("branch-result"),
                    timestamp: Timestamp::from_unix_millis(2),
                },
                CancellationToken::new(),
            )
            .await
            .expect("runtime branch summary");
        assert_eq!(result.usage, Some(usage(8, 4)));
        assert!(result.summary.starts_with(
            "The user explored a different conversation branch before returning here."
        ));
        assert!(
            result
                .summary
                .contains("<modified-files>\nsrc/branch.rs\n</modified-files>")
        );
        assert_eq!(
            result.details.as_ref().unwrap().value.get(),
            r#"{"readFiles":[],"modifiedFiles":["src/branch.rs"]}"#
        );
        {
            let requests = runtime.requests.lock().unwrap();
            assert_eq!(requests[0].options.max_output_tokens, Some(2_048));
            assert_eq!(
                requests[0].options.cache_retention,
                Some(CacheRetention::None)
            );
            let prompt = match &requests[0].context.messages[0] {
                Message::User(message) => match &message.content[0] {
                    ContentBlock::Text { text, .. } => text,
                    _ => panic!("branch summary prompt must be text"),
                },
                _ => panic!("branch summary request must contain one user message"),
            };
            assert!(prompt.contains("Additional focus: preserve decisions"));
        }

        let empty_instruction_runtime = Arc::new(RecordingRuntime::new([
            agentprism_ai::text_response("default prompt with append mode"),
            agentprism_ai::text_response("default prompt with replace mode"),
        ]));
        let empty_instruction_policy = RuntimeBranchSummaryPolicy::new(
            empty_instruction_runtime.clone(),
            ModelRef::new("test", "summary"),
            128_000,
        );
        for replace_instructions in [false, true] {
            empty_instruction_policy
                .summarize(
                    BranchSummaryInput {
                        common_ancestor_id: None,
                        abandoned_entries: vec![SessionEntry::Message {
                            base: EntryBase {
                                id: EntryId::new(format!(
                                    "empty-instructions-message-{replace_instructions}"
                                )),
                                sequence: Sequence::new(1),
                                parent_id: None,
                                timestamp: Timestamp::from_unix_millis(1),
                            },
                            message: user_record(
                                &format!("empty-instructions-{replace_instructions}"),
                                "branch content",
                                1,
                            ),
                            terminate: false,
                        }],
                        target_tail: Vec::new(),
                        custom_instructions: Some(String::new()),
                        replace_instructions,
                        active_model: None,
                        reasoning: agentprism_ai::ReasoningLevel::Off,
                        active_tool_names: Vec::new(),
                        token_budget: 100_000,
                        summary_model: ModelRef::new("test", "summary"),
                        result_entry_id: EntryId::new(format!(
                            "empty-instructions-result-{replace_instructions}"
                        )),
                        timestamp: Timestamp::from_unix_millis(2),
                    },
                    CancellationToken::new(),
                )
                .await
                .expect("empty custom branch instructions use the default prompt");
        }
        {
            let empty_instruction_requests = empty_instruction_runtime.requests.lock().unwrap();
            assert_eq!(empty_instruction_requests.len(), 2);
            for request in empty_instruction_requests.iter() {
                let prompt = message_text(&request.context.messages[0]);
                assert!(prompt.contains(
                    "Create a structured summary of this conversation branch for context when returning later."
                ));
                assert!(!prompt.contains("Additional focus:"));
            }
        }

        let budget_runtime = Arc::new(RecordingRuntime::new([agentprism_ai::text_response(
            "## Goal\nBudgeted branch work",
        )]));
        let budget_policy = RuntimeBranchSummaryPolicy::new(
            budget_runtime.clone(),
            ModelRef::new("test", "summary"),
            128_000,
        );
        budget_policy
            .summarize(
                BranchSummaryInput {
                    common_ancestor_id: None,
                    abandoned_entries: vec![
                        SessionEntry::BranchSummary {
                            base: EntryBase {
                                id: EntryId::new("nested-branch-summary"),
                                sequence: Sequence::new(1),
                                parent_id: None,
                                timestamp: Timestamp::from_unix_millis(1),
                            },
                            from_id: EntryId::new("nested-abandoned"),
                            summary: "ABCD".to_owned(),
                            details: None,
                            usage: None,
                        },
                        SessionEntry::Message {
                            base: EntryBase {
                                id: EntryId::new("nine-token-tail"),
                                sequence: Sequence::new(2),
                                parent_id: Some(EntryId::new("nested-branch-summary")),
                                timestamp: Timestamp::from_unix_millis(2),
                            },
                            message: user_record(
                                "nine-token-tail",
                                "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                                2,
                            ),
                            terminate: false,
                        },
                    ],
                    target_tail: Vec::new(),
                    custom_instructions: None,
                    replace_instructions: false,
                    active_model: None,
                    reasoning: agentprism_ai::ReasoningLevel::Off,
                    active_tool_names: Vec::new(),
                    token_budget: 10,
                    summary_model: ModelRef::new("test", "summary"),
                    result_entry_id: EntryId::new("budgeted-branch-result"),
                    timestamp: Timestamp::from_unix_millis(3),
                },
                CancellationToken::new(),
            )
            .await
            .expect("summary role fits the exact abandoned-branch token budget");
        let budget_requests = budget_runtime.requests.lock().unwrap();
        let budget_prompt = message_text(&budget_requests[0].context.messages[0]);
        assert!(budget_prompt.contains("ABCD"));
        assert!(budget_prompt.contains(BRANCH_SUMMARY_PREFIX));
    });
}

#[test]
fn branch_summary_records_from_id() {
    // Pi basis: packages/agent/test/harness/reducer.test.ts navigation fixtures pair
    // sourceLeafId with the branch-summary entry's fromId.
    block_on(async {
        let storage = branched_storage();
        navigator(storage.clone(), Arc::new(RecordingBranchPolicy::new(0)))
            .navigate(
                Some(EntryId::new("target-1")),
                true,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(storage.state_snapshot().unwrap().entries_in_sequence_order().iter().any(|entry| {
            matches!(entry, SessionEntry::BranchSummary { from_id, .. } if from_id == &EntryId::new("abandoned-2"))
        }));
    });
}

#[test]
fn branch_summary_navigation_is_durable() {
    // Pi basis: packages/agent/src/harness session branch-tree protocol and branch summarization helper.
    block_on(async {
        let storage = branched_storage();
        let result = navigator(storage.clone(), Arc::new(RecordingBranchPolicy::new(0)))
            .navigate(
                Some(EntryId::new("target-1")),
                true,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let state = storage.state_snapshot().unwrap();
        assert_eq!(
            state.lane_leaf(&LaneName::new("main")),
            Some(&result.new_leaf_id)
        );
        assert!(matches!(
            state.recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Idle
        ));

        let root_move_storage = branched_storage();
        let root_move = navigator(
            root_move_storage.clone(),
            Arc::new(RecordingBranchPolicy::new(0)),
        )
        .navigate(None, false, None, CancellationToken::new())
        .await
        .expect("navigate to empty root without summary");
        assert_eq!(root_move.new_leaf_id, None);
        assert_eq!(
            root_move_storage
                .state_snapshot()
                .unwrap()
                .lane_leaf(&LaneName::new("main")),
            Some(&None)
        );

        let root_summary_storage = branched_storage();
        let root_summary = navigator(
            root_summary_storage.clone(),
            Arc::new(RecordingBranchPolicy::new(0)),
        )
        .navigate(None, true, None, CancellationToken::new())
        .await
        .expect("summarize abandoned branch at empty root");
        let root_summary_id = root_summary.summary_entry_id.unwrap();
        let root_state = root_summary_storage.state_snapshot().unwrap();
        assert_eq!(
            root_state.lane_leaf(&LaneName::new("main")),
            Some(&Some(root_summary_id.clone()))
        );
        assert!(matches!(
            root_state.entry(&root_summary_id),
            Some(SessionEntry::BranchSummary { base, from_id, .. })
                if base.parent_id.is_none() && from_id == &EntryId::new("abandoned-2")
        ));
    });
}

#[test]
fn branch_summary_failure_leaves_navigation_recoverable() {
    // Pi basis: packages/agent/src/harness session operation recovery plus
    // branch-summarization and move-first navigation durable prefixes.
    block_on(async {
        let storage = branched_storage();
        let policy = Arc::new(RecordingBranchPolicy::new(1));
        let failed_navigator = navigator(storage.clone(), policy);
        assert!(
            failed_navigator
                .navigate(
                    Some(EntryId::new("target-1")),
                    true,
                    Some("focus".into()),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        let failed = storage.state_snapshot().unwrap();
        assert_eq!(
            failed.lane_leaf(&LaneName::new("main")),
            Some(&Some(EntryId::new("abandoned-2")))
        );
        assert!(matches!(
            failed.recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Resume { .. }
        ));
        failed_navigator
            .resume(CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(
            storage
                .state_snapshot()
                .unwrap()
                .recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Idle
        ));

        let committed_storage = branched_storage();
        append_operation_started(
            &committed_storage,
            "main",
            "committed-navigation",
            Some("abandoned-2"),
            OperationIntent::Navigation {
                target_id: Some(EntryId::new("target-1")),
                summarize: true,
                custom_instructions: Some("focus".to_owned()),
                label: None,
                summary_entry_id: Some(EntryId::new("committed-summary")),
            },
        );
        let committed_state = committed_storage.state_snapshot().unwrap();
        let attempt_sequence = committed_state.next_sequence().unwrap();
        let lane_sequence = Sequence::new(attempt_sequence.get() + 1);
        let entry_sequence = Sequence::new(attempt_sequence.get() + 2);
        committed_storage
            .append_batch(
                committed_state.sequence(),
                vec![
                    SessionMutation::Record {
                        record: OperationRecord::StepAttempt {
                            base: operation_base(
                                "committed-navigation-attempt",
                                "main",
                                attempt_sequence,
                            ),
                            run_id: RunId::new("committed-navigation"),
                            step: OperationStep::BranchSummary,
                            attempt: 1,
                            result_entry_id: EntryId::new("committed-summary"),
                            compaction_reason: None,
                        },
                    },
                    SessionMutation::Lane {
                        sequence: lane_sequence,
                        lane: LaneName::new("main"),
                        leaf_id: Some(EntryId::new("target-1")),
                    },
                    SessionMutation::Entry {
                        lane: Some(LaneName::new("main")),
                        entry: SessionEntry::BranchSummary {
                            base: EntryBase {
                                id: EntryId::new("committed-summary"),
                                sequence: entry_sequence,
                                parent_id: Some(EntryId::new("target-1")),
                                timestamp: Timestamp::from_unix_millis(20),
                            },
                            from_id: EntryId::new("abandoned-2"),
                            summary: "already committed".to_owned(),
                            details: None,
                            usage: None,
                        },
                    },
                ],
            )
            .unwrap();
        let committed_policy = Arc::new(RecordingBranchPolicy::new(1));
        let committed_result = navigator(committed_storage.clone(), committed_policy.clone())
            .resume(CancellationToken::new())
            .await
            .expect("committed navigation target closes without regeneration");
        assert_eq!(
            committed_result.summary_entry_id,
            Some(EntryId::new("committed-summary"))
        );
        assert!(committed_policy.abandoned.lock().unwrap().is_empty());
        assert!(matches!(
            committed_storage
                .state_snapshot()
                .unwrap()
                .recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Idle
        ));

        let moved_storage = branched_storage();
        append_operation_started(
            &moved_storage,
            "main",
            "moved-navigation",
            Some("abandoned-2"),
            OperationIntent::Navigation {
                target_id: Some(EntryId::new("target-1")),
                summarize: false,
                custom_instructions: None,
                label: None,
                summary_entry_id: None,
            },
        );
        let moved_state = moved_storage.state_snapshot().unwrap();
        moved_storage
            .append_batch(
                moved_state.sequence(),
                vec![SessionMutation::Lane {
                    sequence: moved_state.next_sequence().unwrap(),
                    lane: LaneName::new("main"),
                    leaf_id: Some(EntryId::new("target-1")),
                }],
            )
            .unwrap();
        let moved_result = navigator(
            moved_storage.clone(),
            Arc::new(RecordingBranchPolicy::new(1)),
        )
        .resume(CancellationToken::new())
        .await
        .expect("move-first unsummarized navigation closes without moving again");
        assert_eq!(moved_result.new_leaf_id, Some(EntryId::new("target-1")));
        assert!(matches!(
            moved_storage
                .state_snapshot()
                .unwrap()
                .recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Idle
        ));

        let root_recovery_storage = branched_storage();
        let root_recovery_navigator = navigator(
            root_recovery_storage.clone(),
            Arc::new(RecordingBranchPolicy::new(1)),
        );
        assert!(
            root_recovery_navigator
                .navigate(
                    None,
                    true,
                    Some("root focus".into()),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert_eq!(
            root_recovery_storage
                .state_snapshot()
                .unwrap()
                .lane_leaf(&LaneName::new("main")),
            Some(&Some(EntryId::new("abandoned-2")))
        );
        let root_recovered = root_recovery_navigator
            .resume(CancellationToken::new())
            .await
            .expect("root summary navigation resumes");
        let root_recovered_id = root_recovered.summary_entry_id.unwrap();
        assert!(matches!(
            root_recovery_storage
                .state_snapshot()
                .unwrap()
                .entry(&root_recovered_id),
            Some(SessionEntry::BranchSummary { base, from_id, .. })
                if base.parent_id.is_none() && from_id == &EntryId::new("abandoned-2")
        ));

        let local_storage = rc_only_branched_storage();
        let local_policy = Rc::new(LocalRecordingBranchPolicy::new(1));
        let local_root_navigator = local_navigator(local_storage.clone(), local_policy.clone());
        assert!(
            local_root_navigator
                .navigate(
                    None,
                    true,
                    Some("local root".into()),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert_eq!(
            local_storage
                .inner
                .state_snapshot()
                .unwrap()
                .lane_leaf(&LaneName::new("main")),
            Some(&Some(EntryId::new("local-abandoned")))
        );
        let local_recovered = local_root_navigator
            .resume(CancellationToken::new())
            .await
            .expect("Rc-only local root navigation resumes");
        assert_eq!(
            local_policy.abandoned.borrow().as_slice(),
            &[
                vec![EntryId::new("local-root"), EntryId::new("local-abandoned")],
                vec![EntryId::new("local-root"), EntryId::new("local-abandoned")],
            ]
        );
        let local_recovered_id = local_recovered.summary_entry_id.unwrap();
        let local_state = local_storage.inner.state_snapshot().unwrap();
        assert!(matches!(
            local_state.entry(&local_recovered_id),
            Some(SessionEntry::BranchSummary { base, from_id, .. })
                if base.parent_id.is_none() && from_id == &EntryId::new("local-abandoned")
        ));
        assert!(matches!(
            local_state.recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Idle
        ));
    });
}

struct QueueAssistantStep(Mutex<VecDeque<AssistantMessage>>);

impl AssistantStep for QueueAssistantStep {
    fn execute(
        &self,
        _input: AssistantStepInput,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantMessage, HarnessOperationError>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| HarnessOperationError::AssistantStep {
                    message: "no scripted assistant response".into(),
                })
        })
    }
}

struct LocalQueueAssistantStep(RefCell<VecDeque<AssistantMessage>>);

impl LocalAssistantStep for LocalQueueAssistantStep {
    fn execute(
        &self,
        _input: AssistantStepInput,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AssistantMessage, HarnessOperationError>> {
        Box::pin(async move {
            self.0
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| HarnessOperationError::AssistantStep {
                    message: "no scripted local assistant response".into(),
                })
        })
    }
}

struct RecordingInputStep {
    response: Mutex<Option<AssistantMessage>>,
    prepared_records: Mutex<Vec<Vec<AgentRecord>>>,
}

impl AssistantStep for RecordingInputStep {
    fn execute(
        &self,
        input: AssistantStepInput,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantMessage, HarnessOperationError>> {
        Box::pin(async move {
            self.prepared_records
                .lock()
                .unwrap()
                .push(input.prepared.records);
            self.response.lock().unwrap().take().ok_or_else(|| {
                HarnessOperationError::AssistantStep {
                    message: "missing recording response".into(),
                }
            })
        })
    }
}

struct LocalRecordingInputStep {
    response: RefCell<Option<AssistantMessage>>,
    prepared_records: RefCell<Vec<Vec<AgentRecord>>>,
}

impl LocalAssistantStep for LocalRecordingInputStep {
    fn execute(
        &self,
        input: AssistantStepInput,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AssistantMessage, HarnessOperationError>> {
        Box::pin(async move {
            self.prepared_records
                .borrow_mut()
                .push(input.prepared.records);
            self.response
                .borrow_mut()
                .take()
                .ok_or_else(|| HarnessOperationError::AssistantStep {
                    message: "missing local recording response".into(),
                })
        })
    }
}

fn explicit_overflow_message(id: &str) -> AssistantMessage {
    assistant_message(
        id,
        "",
        AssistantFinishReason::Error,
        Some(PublicError {
            code: "provider_error".into(),
            message: "prompt is too long".into(),
            retryable: false,
            provider_code: None,
            status: Some(400),
            request_id: None,
        }),
        usage(100, 0),
    )
}

fn assert_exhausted_outcome(storage: &InMemorySessionStorage) {
    let state = storage.state_snapshot().unwrap();
    assert!(state.records_in_sequence_order().iter().any(|record| {
        matches!(
            record,
            OperationRecord::Finished {
                outcome: OperationOutcome::Failed,
                error: Some(error),
                ..
            } if error.code == "context_overflow_recovery_exhausted"
        )
    }));
    assert!(matches!(
        state.recovery_decision(&LaneName::new("main")),
        RecoveryDecision::Idle
    ));
}

#[test]
fn compaction_overflow_reason() {
    // Pi basis: packages/agent/test/harness/reducer.test.ts, "overflow compaction and retry" valid prefix.
    block_on(async {
        let primary_storage = storage();
        append_message(
            &primary_storage,
            "main",
            "u1",
            None,
            user_record("u1", "large prompt", 1),
        );
        let session = harness_session(primary_storage.clone(), "main");
        let compaction = Arc::new(FixedCompactionPolicy::only_forced());
        let context = Arc::new(context_policy(session.clone(), compaction.clone()));
        let executor = OverflowRetryExecutor {
            context_policy: context,
            session,
            context_window: Some(100),
        };
        let overflow = explicit_overflow_message("overflow");
        let success = assistant_message(
            "success",
            "fits",
            AssistantFinishReason::Stop,
            None,
            usage(20, 2),
        );
        let step = QueueAssistantStep(Mutex::new(VecDeque::from([overflow, success])));
        let result = executor
            .run(
                state_view(&agent_state()),
                OverflowRunIntent::default(),
                &step,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(result.recovered_from_overflow);
        assert_eq!(
            compaction.observed_reasons.lock().unwrap().as_slice(),
            &[None, Some(CompactionReason::Overflow)]
        );
        let state = primary_storage.state_snapshot().unwrap();
        let run_ids = state
            .records_in_sequence_order()
            .iter()
            .filter_map(OperationRecord::run_id)
            .collect::<Vec<_>>();
        assert!(run_ids.iter().all(|run_id| run_id == &result.run_id));
        assert!(
            state
                .records_in_sequence_order()
                .iter()
                .any(|record| matches!(
                    record,
                    OperationRecord::StepAttempt {
                        step: OperationStep::Compaction,
                        compaction_reason: Some(CompactionReason::Overflow),
                        ..
                    }
                ))
        );

        // Pi basis: packages/agent/test/harness/reducer.test.ts one-tool run X1-X5;
        // provisioned initial messages are committed before the assistant attempt.
        let send_input_storage = storage();
        let send_input_session = harness_session(send_input_storage.clone(), "main");
        let send_input_context = Arc::new(context_policy(
            send_input_session.clone(),
            Arc::new(FixedCompactionPolicy::only_forced()),
        ));
        let send_prompt = user_record("send-initial-message", "real prompt", 1);
        let send_step = RecordingInputStep {
            response: Mutex::new(Some(assistant_message(
                "send-initial-assistant",
                "answer",
                AssistantFinishReason::Stop,
                None,
                usage(4, 1),
            ))),
            prepared_records: Mutex::new(Vec::new()),
        };
        OverflowRetryExecutor {
            context_policy: send_input_context,
            session: send_input_session,
            context_window: Some(100),
        }
        .run(
            state_view(&agent_state()),
            OverflowRunIntent {
                original_prompt: vec![send_prompt.clone()],
                initial_messages: vec![ProvisionedEntry::Message {
                    id: EntryId::new("send-initial-entry"),
                    message: send_prompt,
                    terminate: false,
                }],
                system_prompt_override: None,
            },
            &send_step,
            CancellationToken::new(),
        )
        .await
        .expect("send initial input run");
        assert_eq!(send_step.prepared_records.lock().unwrap()[0].len(), 1);
        let send_input_state = send_input_storage.state_snapshot().unwrap();
        let send_entries = send_input_state
            .entries_in_sequence_order()
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(send_entries[0].id(), &EntryId::new("send-initial-entry"));
        assert_eq!(
            send_entries[1].parent_id(),
            Some(&EntryId::new("send-initial-entry"))
        );

        let local_input_storage = local_storage();
        let local_input_session = local_harness_session(local_input_storage.clone(), "main");
        let local_input_context = Rc::new(local_context_policy(
            local_input_session.clone(),
            Rc::new(FixedCompactionPolicy::only_forced()),
        ));
        let local_prompt = user_record("local-initial-message", "local real prompt", 1);
        let local_step = LocalRecordingInputStep {
            response: RefCell::new(Some(assistant_message(
                "local-initial-assistant",
                "answer",
                AssistantFinishReason::Stop,
                None,
                usage(4, 1),
            ))),
            prepared_records: RefCell::new(Vec::new()),
        };
        LocalOverflowRetryExecutor {
            context_policy: local_input_context,
            session: local_input_session,
            context_window: Some(100),
        }
        .run(
            state_view(&agent_state()),
            OverflowRunIntent {
                original_prompt: vec![local_prompt.clone()],
                initial_messages: vec![ProvisionedEntry::Message {
                    id: EntryId::new("local-initial-entry"),
                    message: local_prompt,
                    terminate: false,
                }],
                system_prompt_override: None,
            },
            &local_step,
            CancellationToken::new(),
        )
        .await
        .expect("local initial input run");
        assert_eq!(local_step.prepared_records.borrow()[0].len(), 1);
        let local_input_state = local_input_storage.state_snapshot().unwrap();
        let local_entries = local_input_state
            .entries_in_sequence_order()
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(local_entries[0].id(), &EntryId::new("local-initial-entry"));
        assert_eq!(
            local_entries[1].parent_id(),
            Some(&EntryId::new("local-initial-entry"))
        );

        for (suffix, terminal) in [
            (
                "stop",
                assistant_message(
                    "silent-stop",
                    "",
                    AssistantFinishReason::Stop,
                    None,
                    usage(101, 0),
                ),
            ),
            (
                "length",
                assistant_message(
                    "silent-length",
                    "",
                    AssistantFinishReason::Length,
                    None,
                    usage(99, 0),
                ),
            ),
        ] {
            let send_storage = storage();
            append_message(
                &send_storage,
                "main",
                &format!("send-user-{suffix}"),
                None,
                user_record(&format!("send-user-{suffix}"), "large prompt", 1),
            );
            let send_session = harness_session(send_storage.clone(), "main");
            let send_context = Arc::new(context_policy(
                send_session.clone(),
                Arc::new(FixedCompactionPolicy::only_forced()),
            ));
            let send_executor = OverflowRetryExecutor {
                context_policy: send_context,
                session: send_session,
                context_window: Some(100),
            };
            let send_step = QueueAssistantStep(Mutex::new(VecDeque::from([
                explicit_overflow_message(&format!("send-first-{suffix}")),
                terminal.clone(),
            ])));
            assert_eq!(
                send_executor
                    .run(
                        state_view(&agent_state()),
                        OverflowRunIntent::default(),
                        &send_step,
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap_err(),
                HarnessOperationError::OverflowRecoveryExhausted
            );
            assert_exhausted_outcome(&send_storage);

            let local_storage = local_storage();
            append_message(
                &local_storage,
                "main",
                &format!("local-user-{suffix}"),
                None,
                user_record(&format!("local-user-{suffix}"), "large prompt", 1),
            );
            let local_session = local_harness_session(local_storage.clone(), "main");
            let local_context = Rc::new(local_context_policy(
                local_session.clone(),
                Rc::new(FixedCompactionPolicy::only_forced()),
            ));
            let local_executor = LocalOverflowRetryExecutor {
                context_policy: local_context,
                session: local_session,
                context_window: Some(100),
            };
            let local_step = LocalQueueAssistantStep(RefCell::new(VecDeque::from([
                explicit_overflow_message(&format!("local-first-{suffix}")),
                terminal,
            ])));
            assert_eq!(
                local_executor
                    .run(
                        state_view(&agent_state()),
                        OverflowRunIntent::default(),
                        &local_step,
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap_err(),
                HarnessOperationError::OverflowRecoveryExhausted
            );
            assert_exhausted_outcome(&local_storage);
        }
    });
}
