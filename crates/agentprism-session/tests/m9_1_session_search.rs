//! Format-agnostic session search behavior against memory and file backends.

use agentprism_ai::{CancellationToken, Timestamp, VersionedExtension};
use agentprism_session::{
    CreateSessionRequest, EntryBase, EntryId, FileSessionRepository, InMemorySessionRepository,
    LaneName, Sequence, SessionEntry, SessionEntryKind, SessionEnvironmentMetadata,
    SessionErrorKind, SessionFact, SessionId, SessionMutation, SessionRepository,
    SessionSearchOptions, SessionStorage, search_session_repository, search_session_storages,
};
use futures_executor::block_on;
use serde_json::value::RawValue;
use std::{collections::BTreeSet, sync::Arc};

fn request(id: &str, created_at: i64) -> CreateSessionRequest {
    CreateSessionRequest::new(
        id,
        Timestamp::from_unix_millis(created_at),
        SessionEnvironmentMetadata::default(),
    )
}

fn custom_entry(sequence: u64, id: &str, text: &str) -> SessionMutation {
    SessionMutation::Entry {
        lane: Some(LaneName::new("main")),
        entry: SessionEntry::Custom {
            base: EntryBase {
                id: EntryId::new(id),
                sequence: Sequence::new(sequence),
                parent_id: None,
                timestamp: Timestamp::from_unix_millis(sequence as i64),
            },
            custom_type: "note".to_owned(),
            data: Some(VersionedExtension {
                schema_version: 1,
                value: RawValue::from_string(format!("{{\"text\":{}}}", json_string(text)))
                    .unwrap(),
            }),
        },
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

async fn create_search_source(
    repository: &dyn SessionRepository,
    prefix: &str,
) -> Vec<Arc<dyn SessionStorage>> {
    let root = repository
        .create(request(&format!("{prefix}-root"), 1))
        .await
        .unwrap();
    root.append(
        Sequence::ZERO,
        vec![custom_entry(1, "root-entry", "fix auth flow")],
    )
    .await
    .unwrap();
    let other = repository
        .create(request(&format!("{prefix}-other"), 2))
        .await
        .unwrap();
    other
        .append(
            Sequence::ZERO,
            vec![custom_entry(1, "other-entry", "auth in another workspace")],
        )
        .await
        .unwrap();
    vec![root, other]
}

async fn assert_search_behavior(repository: &dyn SessionRepository, prefix: &str) {
    let storages = create_search_source(repository, prefix).await;
    let hits = search_session_storages(
        &storages,
        "auth",
        SessionSearchOptions::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| hit.session_id.as_str())
            .collect::<Vec<_>>(),
        [format!("{prefix}-root"), format!("{prefix}-other")]
    );
    assert!(
        search_session_storages(
            &storages,
            "missing",
            SessionSearchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .is_empty()
    );

    let state = storages[0].load_state().await.unwrap();
    let next = state.next_sequence().unwrap();
    storages[0]
        .append(
            state.sequence(),
            vec![SessionMutation::Fact {
                sequence: next,
                fact: SessionFact::Label {
                    target_id: EntryId::new("root-entry"),
                    label: Some("important label".to_owned()),
                },
            }],
        )
        .await
        .unwrap();
    let label_hits = search_session_storages(
        &storages,
        "important",
        SessionSearchOptions::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(label_hits.len(), 1);
    assert_eq!(label_hits[0].entry_id, EntryId::new("root-entry"));

    let no_custom = search_session_storages(
        &storages,
        "auth",
        SessionSearchOptions {
            entry_kinds: Some(BTreeSet::from([SessionEntryKind::Message])),
            limit: None,
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(no_custom.is_empty());

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = search_session_storages(
        &storages,
        "auth",
        SessionSearchOptions::default(),
        cancellation,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, SessionErrorKind::Cancelled);
}

#[test]
fn session_search_scans_arbitrary_sources_on_both_backends() {
    // Pi basis: packages/agent/test/harness/session/search.test.ts, arbitrary
    // projected source, labels, type filters, and cancellation cases.
    block_on(async {
        let memory = InMemorySessionRepository::new();
        assert_search_behavior(&memory, "memory-search").await;

        let directory = tempfile::tempdir().unwrap();
        let file = FileSessionRepository::new(directory.path()).unwrap();
        assert_search_behavior(&file, "file-search").await;
    });
}

#[test]
fn session_search_scans_repository_on_both_backends() {
    // Pi basis: packages/agent/test/harness/session/search.test.ts, disk-backed
    // scanning source; the native backend replaces Pi-v4 JSONL bytes.
    block_on(async {
        let memory = InMemorySessionRepository::new();
        create_search_source(&memory, "memory-repository").await;
        let memory_hits = search_session_repository(
            &memory,
            "auth",
            SessionSearchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(memory_hits.len(), 2);

        let directory = tempfile::tempdir().unwrap();
        let file = FileSessionRepository::new(directory.path()).unwrap();
        create_search_source(&file, "file-repository").await;
        let file_hits = search_session_repository(
            &file,
            "auth",
            SessionSearchOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(file_hits.len(), 2);
        assert!(file_hits.iter().all(|hit| {
            matches!(
                hit.session_id,
                ref id if id == &SessionId::new("file-repository-root")
                    || id == &SessionId::new("file-repository-other")
            )
        }));
    });
}
