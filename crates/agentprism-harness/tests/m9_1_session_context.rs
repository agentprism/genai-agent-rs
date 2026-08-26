//! Format-agnostic session-context behavior against memory and file backends.

use agentprism_ai::{
    ApiId, AssistantFinish, AssistantFinishReason, AssistantMessage, CancellationToken,
    ContentBlock, ContentBlockId, Message, MessageId, ModelId, ModelRef, ProviderId,
    ReasoningLevel, ReplayEnvelope, ReplayScope, SendBoxFuture, Timestamp, Usage, UsageSource,
    UserMessage, VersionedExtension,
};
use agentprism_core::AgentRecord;
use agentprism_env::{Clock, ClockError};
use agentprism_harness::{
    CompactionError, CustomSessionEntryProjector, LocalSessionContextEntryTransform,
    MonotonicHarnessIdGenerator, OmitCustomSessionEntries, Session, SessionContextEntryTransform,
    reconstruct_branch_context, reconstruct_branch_context_with_local_options,
    reconstruct_branch_context_with_options, reconstruct_branch_context_with_projector,
};
use agentprism_session::{
    CreateSessionRequest, EntryBase, EntryId, FileSessionRepository, InMemorySessionRepository,
    LaneName, Sequence, SessionEntry, SessionEnvironmentMetadata, SessionMutation,
    SessionRepository,
};
use futures_executor::block_on;
use serde_json::value::RawValue;
use std::{rc::Rc, sync::Arc, time::Duration};

#[derive(Clone, Copy)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(1)
    }

    fn sleep(
        &self,
        _duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), ClockError>> {
        Box::pin(async move { cancellation.check().map_err(|_| ClockError::Cancelled) })
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

fn assistant_record(id: &str, text: &str, reason: AssistantFinishReason) -> AgentRecord {
    let provider = ProviderId::new("anthropic");
    let api = ApiId::new("anthropic-messages");
    let model = ModelId::new("claude-sonnet-4-5");
    AgentRecord::Llm(Message::Assistant(AssistantMessage {
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
        usage: Usage::zero(UsageSource::ProviderReported),
        cost: None,
        finish: AssistantFinish {
            reason,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: Timestamp::from_unix_millis(1),
    }))
}

fn base(sequence: u64, id: &str, parent_id: Option<&str>) -> EntryBase {
    EntryBase {
        id: EntryId::new(id),
        sequence: Sequence::new(sequence),
        parent_id: parent_id.map(EntryId::new),
        timestamp: Timestamp::from_unix_millis(sequence as i64),
    }
}

fn mutation(entry: SessionEntry) -> SessionMutation {
    SessionMutation::Entry {
        lane: Some(LaneName::new("main")),
        entry,
    }
}

fn request(id: &str) -> CreateSessionRequest {
    CreateSessionRequest::new(
        id,
        Timestamp::from_unix_millis(1),
        SessionEnvironmentMetadata::default(),
    )
}

async fn append_and_path(
    repository: &dyn SessionRepository,
    id: &str,
    mutations: Vec<SessionMutation>,
) -> Vec<SessionEntry> {
    let storage = repository.create(request(id)).await.unwrap();
    storage.append(Sequence::ZERO, mutations).await.unwrap();
    let state = storage.load_state().await.unwrap();
    let leaf = state
        .lane_leaf(&LaneName::new("main"))
        .unwrap()
        .as_ref()
        .unwrap();
    state
        .scan_branch_root_to_leaf(leaf)
        .unwrap()
        .into_iter()
        .cloned()
        .collect()
}

fn serialized_records(records: &[AgentRecord]) -> String {
    serde_json::to_string(records).unwrap()
}

async fn assert_latest_compaction_behavior(repository: &dyn SessionRepository, id: &str) {
    let path = append_and_path(
        repository,
        id,
        vec![
            mutation(SessionEntry::Message {
                base: base(1, "old", None),
                message: user_record("old-message", "old", 1),
                terminate: false,
            }),
            mutation(SessionEntry::Compaction {
                base: base(2, "compact", Some("old")),
                summary: "summary".to_owned(),
                retained_tail: vec![
                    user_record("retained", "retained", 2),
                    assistant_record("answer", "answer", AssistantFinishReason::Stop),
                ],
                tokens_before: 100,
                details: None,
                usage: None,
            }),
            mutation(SessionEntry::ModelChange {
                base: base(3, "model", Some("compact")),
                model: ModelRef::new("openai", "gpt-5"),
            }),
            mutation(SessionEntry::ReasoningChange {
                base: base(4, "reasoning", Some("model")),
                level: ReasoningLevel::High,
            }),
            mutation(SessionEntry::Message {
                base: base(5, "tail", Some("reasoning")),
                message: user_record("tail-message", "tail", 5),
                terminate: false,
            }),
        ],
    )
    .await;
    let context = reconstruct_branch_context(&path).unwrap();
    let rendered = serialized_records(&context.records);
    assert!(rendered.contains("summary"));
    assert!(rendered.contains("retained"));
    assert!(rendered.contains("answer"));
    assert!(rendered.contains("tail"));
    assert!(!rendered.contains("old-message"));
    assert_eq!(context.model, Some(ModelRef::new("openai", "gpt-5")));
    assert_eq!(context.reasoning, ReasoningLevel::High);
}

#[derive(Default)]
struct NoteProjector;

impl CustomSessionEntryProjector for NoteProjector {
    fn project(
        &self,
        entry: &SessionEntry,
        index: usize,
        path: &[SessionEntry],
    ) -> Result<Vec<AgentRecord>, CompactionError> {
        assert_eq!(index, 1);
        assert_eq!(
            path.iter()
                .map(|entry| entry.id().as_str())
                .collect::<Vec<_>>(),
            ["compact", "custom", "deferred"]
        );
        let SessionEntry::Custom {
            data: Some(data), ..
        } = entry
        else {
            return Ok(Vec::new());
        };
        Ok(vec![user_record(
            "projected-note",
            &format!("note: {}", data.value.get()),
            3,
        )])
    }
}

async fn assert_transform_and_projector_behavior(repository: &dyn SessionRepository, id: &str) {
    let path = append_and_path(
        repository,
        id,
        vec![
            mutation(SessionEntry::Message {
                base: base(1, "old", None),
                message: user_record("old-message", "old", 1),
                terminate: false,
            }),
            mutation(SessionEntry::Compaction {
                base: base(2, "compact", Some("old")),
                summary: "summary".to_owned(),
                retained_tail: Vec::new(),
                tokens_before: 100,
                details: None,
                usage: None,
            }),
            mutation(SessionEntry::Custom {
                base: base(3, "custom", Some("compact")),
                custom_type: "note".to_owned(),
                data: Some(VersionedExtension {
                    schema_version: 1,
                    value: RawValue::from_string("\"project me\"".to_owned()).unwrap(),
                }),
            }),
            mutation(SessionEntry::Message {
                base: base(4, "deferred", Some("custom")),
                message: assistant_record("deferred-message", "", AssistantFinishReason::Deferred),
                terminate: false,
            }),
        ],
    )
    .await;
    let context = reconstruct_branch_context_with_projector(&path, &NoteProjector).unwrap();
    let rendered = serialized_records(&context.records);
    assert!(rendered.contains("summary"));
    assert!(rendered.contains("project me"));
    assert!(!rendered.contains("deferred-message"));
    assert!(!rendered.contains("old-message"));
}

#[derive(Default)]
struct RemoveCompactionTransform;

impl SessionContextEntryTransform for RemoveCompactionTransform {
    fn transform(&self, entries: &[SessionEntry]) -> Result<Vec<SessionEntry>, CompactionError> {
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id().as_str())
                .collect::<Vec<_>>(),
            ["compact", "branch", "tail"]
        );
        Ok(entries
            .iter()
            .filter(|entry| !matches!(entry, SessionEntry::Compaction { .. }))
            .cloned()
            .collect())
    }
}

impl LocalSessionContextEntryTransform for RemoveCompactionTransform {
    fn transform(&self, entries: &[SessionEntry]) -> Result<Vec<SessionEntry>, CompactionError> {
        SessionContextEntryTransform::transform(self, entries)
    }
}

async fn assert_caller_transform_behavior(repository: &dyn SessionRepository, id: &str) {
    let path = append_and_path(
        repository,
        id,
        vec![
            mutation(SessionEntry::Message {
                base: base(1, "old", None),
                message: user_record("old-message", "old", 1),
                terminate: false,
            }),
            mutation(SessionEntry::Compaction {
                base: base(2, "compact", Some("old")),
                summary: "compaction summary".to_owned(),
                retained_tail: Vec::new(),
                tokens_before: 100,
                details: None,
                usage: None,
            }),
            mutation(SessionEntry::BranchSummary {
                base: base(3, "branch", Some("compact")),
                from_id: EntryId::new("abandoned"),
                summary: "branch summary".to_owned(),
                details: None,
                usage: None,
            }),
            mutation(SessionEntry::Message {
                base: base(4, "tail", Some("branch")),
                message: user_record("tail-message", "tail", 4),
                terminate: false,
            }),
        ],
    )
    .await;
    let transforms: Vec<Arc<dyn SessionContextEntryTransform>> =
        vec![Arc::new(RemoveCompactionTransform)];
    let context =
        reconstruct_branch_context_with_options(&path, &transforms, &OmitCustomSessionEntries)
            .unwrap();
    let rendered = serialized_records(&context.records);
    assert!(rendered.contains("branch summary"));
    assert!(rendered.contains("tail"));
    assert!(!rendered.contains("compaction summary"));
    assert!(!rendered.contains("old-message"));

    let local_transforms: Vec<Rc<dyn LocalSessionContextEntryTransform>> =
        vec![Rc::new(RemoveCompactionTransform)];
    let local = reconstruct_branch_context_with_local_options(
        &path,
        &local_transforms,
        &OmitCustomSessionEntries,
    )
    .unwrap();
    assert_eq!(serialized_records(&local.records), rendered);
}

#[test]
fn session_context_latest_compaction_behavior_on_both_backends() {
    // Pi basis: packages/agent/test/harness/session/context.test.ts, latest
    // compaction retained-tail and durable model/reasoning state case.
    block_on(async {
        let memory = InMemorySessionRepository::new();
        assert_latest_compaction_behavior(&memory, "memory-context-latest").await;

        let directory = tempfile::tempdir().unwrap();
        let file = FileSessionRepository::new(directory.path()).unwrap();
        assert_latest_compaction_behavior(&file, "file-context-latest").await;
    });
}

#[test]
fn session_context_projector_and_deferred_behavior_on_both_backends() {
    // Pi basis: packages/agent/test/harness/session/context.test.ts, custom
    // projection runs on the compacted path and omits deferred handles.
    block_on(async {
        let memory = InMemorySessionRepository::new();
        assert_transform_and_projector_behavior(&memory, "memory-context-projector").await;

        let directory = tempfile::tempdir().unwrap();
        let file = FileSessionRepository::new(directory.path()).unwrap();
        assert_transform_and_projector_behavior(&file, "file-context-projector").await;
    });
}

#[test]
fn session_context_caller_transform_after_compaction_boundary_on_both_backends() {
    // Pi basis: packages/agent/test/harness/session/context.test.ts, "applies
    // caller transforms after the compaction boundary".
    block_on(async {
        let memory = InMemorySessionRepository::new();
        assert_caller_transform_behavior(&memory, "memory-context-transform").await;

        let directory = tempfile::tempdir().unwrap();
        let file = FileSessionRepository::new(directory.path()).unwrap();
        assert_caller_transform_behavior(&file, "file-context-transform").await;
    });
}

async fn assert_shared_id_generator(repository: &dyn SessionRepository, id: &str) {
    let storage = repository.create(request(id)).await.unwrap();
    storage
        .append(
            Sequence::ZERO,
            vec![SessionMutation::Lane {
                sequence: Sequence::FIRST,
                lane: LaneName::new("thread"),
                leaf_id: None,
            }],
        )
        .await
        .unwrap();
    let ids = Arc::new(MonotonicHarnessIdGenerator::new("generated"));
    let clock = Arc::new(FixedClock);
    let main = Session::new(
        storage.clone(),
        LaneName::new("main"),
        ids.clone(),
        clock.clone(),
    );
    let thread = Session::new(storage, LaneName::new("thread"), ids, clock);
    assert_eq!(main.next_entry_id("entry").as_str(), "generated-entry-1");
    assert_eq!(thread.next_entry_id("entry").as_str(), "generated-entry-2");
}

#[test]
fn session_lane_views_share_injected_id_generator_on_both_backends() {
    // Pi basis: packages/agent/test/harness/session/memory.test.ts, one
    // injectable id generator is shared by lane views.
    block_on(async {
        let memory = InMemorySessionRepository::new();
        assert_shared_id_generator(&memory, "memory-shared-ids").await;

        let directory = tempfile::tempdir().unwrap();
        let file = FileSessionRepository::new(directory.path()).unwrap();
        assert_shared_id_generator(&file, "file-shared-ids").await;
    });
}
