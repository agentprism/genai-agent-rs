//! M9.1 native durable file storage protocol conformance.

use agentprism_ai::{RunId, Timestamp};
use agentprism_session::{
    CreateSessionRequest, EntryBase, EntryId, FileSessionRepository, FileSessionStorage,
    ForkPosition, ForkRequest, LaneName, OperationIntent, OperationRecord, OperationRecordBase,
    OperationRecordId, RecoveryDecision, Sequence, SessionEntry, SessionEnvironmentMetadata,
    SessionErrorKind, SessionId, SessionMutation, SessionRepository,
};
use futures_executor::block_on;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    sync::{Arc, Barrier},
    thread,
};

fn request(id: &str, created_at: i64) -> CreateSessionRequest {
    CreateSessionRequest::new(
        id,
        Timestamp::from_unix_millis(created_at),
        SessionEnvironmentMetadata::default(),
    )
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
                timestamp: Timestamp::from_unix_millis(sequence as i64),
            },
            custom_type: "note".to_owned(),
            data: None,
        },
    }
}

fn started(sequence: u64, id: &str) -> SessionMutation {
    SessionMutation::Record {
        record: OperationRecord::Started {
            base: OperationRecordBase {
                id: OperationRecordId::new(id),
                sequence: Sequence::new(sequence),
                lane: LaneName::new("main"),
                timestamp: Timestamp::from_unix_millis(sequence as i64),
            },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: Vec::new(),
                initial_messages: Vec::new(),
                system_prompt_override: None,
                resume_data: Default::default(),
            },
        },
    }
}

#[test]
fn session_torn_tail_is_repaired_on_open() {
    // §10.10 native storage protocol. Pi basis: harness/session/jsonl/storage.ts
    // repairs a syntactically torn final append by publishing the valid prefix.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    let storage = block_on(SessionRepository::create(&repository, request("torn", 1))).unwrap();
    block_on(storage.append(
        Sequence::ZERO,
        vec![custom_entry(1, "accepted", None, Some("main"))],
    ))
    .unwrap();
    let path = repository.session_path(&SessionId::new("torn")).unwrap();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"mutation_batch\":").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let (reopened, report) = FileSessionStorage::open_with_repair_report(&path).unwrap();
    assert!(report.repaired);
    assert!(report.removed_bytes > 0);
    assert_eq!(report.last_sequence, Sequence::FIRST);
    assert_eq!(
        reopened.load_state_sync().unwrap().sequence(),
        Sequence::FIRST
    );
    let repaired = fs::read(&path).unwrap();
    assert!(repaired.ends_with(b"\n"));
    assert_eq!(
        String::from_utf8(repaired)
            .unwrap()
            .matches("{\"mutation_batch\":")
            .count(),
        1
    );
}

#[test]
fn session_unterminated_valid_tail_is_repaired_on_open() {
    // §10.10 torn-tail normalization. Pi basis: jsonl/storage.ts appends the
    // missing final newline when the final record itself is complete.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    let storage = block_on(SessionRepository::create(
        &repository,
        request("unterminated", 1),
    ))
    .unwrap();
    block_on(storage.append(
        Sequence::ZERO,
        vec![custom_entry(1, "accepted", None, Some("main"))],
    ))
    .unwrap();
    let path = repository
        .session_path(&SessionId::new("unterminated"))
        .unwrap();
    let mut bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.pop(), Some(b'\n'));
    fs::write(&path, bytes).unwrap();

    let (_, report) = FileSessionStorage::open_with_repair_report(&path).unwrap();
    assert!(report.repaired);
    assert_eq!(report.removed_bytes, 0);
    assert!(fs::read(path).unwrap().ends_with(b"\n"));
}

#[test]
fn session_midfile_corruption_is_not_silently_repaired() {
    // §10.10 integrity rule. Pi basis: jsonl/storage.ts repairs only the final
    // syntax failure and rejects interior corruption.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    block_on(SessionRepository::create(
        &repository,
        request("midfile", 1),
    ))
    .unwrap();
    let path = repository.session_path(&SessionId::new("midfile")).unwrap();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"not-json\n{}\n").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let error = FileSessionStorage::open(&path).unwrap_err();
    assert_eq!(error.kind, SessionErrorKind::Corruption);
    assert!(fs::read_to_string(path).unwrap().contains("not-json"));
}

#[test]
fn session_sequence_gap_is_corruption() {
    // §10.10 session_sequence_gap_is_corruption. Pi basis: codec/storage replay
    // validates one consecutive global sequence and never repairs semantic gaps.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    let storage = block_on(SessionRepository::create(
        &repository,
        request("sequence-gap", 1),
    ))
    .unwrap();
    block_on(storage.append(
        Sequence::ZERO,
        vec![custom_entry(1, "root", None, Some("main"))],
    ))
    .unwrap();
    let path = repository
        .session_path(&SessionId::new("sequence-gap"))
        .unwrap();
    let original = fs::read_to_string(&path).unwrap();
    let corrupted = original.replacen("\"sequence\":1", "\"sequence\":2", 1);
    assert_ne!(corrupted, original);
    fs::write(&path, corrupted).unwrap();

    let error = FileSessionStorage::open(&path).unwrap_err();
    assert_eq!(error.kind, SessionErrorKind::Corruption);
    assert!(error.message.contains("non-consecutive sequence"));
}

#[test]
fn session_concurrent_append_is_serialized() {
    // Native storage protocol mirror of Pi JSONL concurrent append behavior:
    // one sidecar OS append lock establishes the only commit order.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    let path = repository
        .session_path(&SessionId::new("concurrent"))
        .unwrap();
    let storage = Arc::new(
        FileSessionStorage::create(&path, request("concurrent", 1).into_header()).unwrap(),
    );
    let barrier = Arc::new(Barrier::new(3));
    let handles = ["left", "right"].map(|id| {
        let storage = storage.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            storage.append_batch_sync(
                Sequence::ZERO,
                vec![custom_entry(1, id, None, Some("main"))],
            )
        })
    });
    barrier.wait();
    let results = handles.map(|handle| handle.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .filter(|error| error.kind == SessionErrorKind::SequenceConflict)
            .count(),
        1
    );
    assert_eq!(
        FileSessionStorage::open(path)
            .unwrap()
            .load_state_sync()
            .unwrap()
            .sequence(),
        Sequence::FIRST
    );
}

#[test]
fn session_open_operation_detected() {
    // §7.4 live/replay split. Pi basis: session/memory.ts and jsonl/storage.ts
    // reject a second live operation start on the same lane.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    let storage = block_on(SessionRepository::create(
        &repository,
        request("one-open", 1),
    ))
    .unwrap();
    block_on(storage.append(Sequence::ZERO, vec![started(1, "first")])).unwrap();
    let error = block_on(storage.append(Sequence::FIRST, vec![started(2, "second")])).unwrap_err();
    assert_eq!(error.kind, SessionErrorKind::Storage);
    let state = block_on(storage.load_state()).unwrap();
    assert_eq!(state.open_operations(&LaneName::new("main")).len(), 1);
}

#[test]
fn session_multiple_open_operations_is_corruption() {
    // §7.4 replay half of the split. A manually corrupted native log retains
    // both unresolved starts so RecoveryDecision diagnoses corruption.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    let storage = block_on(SessionRepository::create(
        &repository,
        request("corrupt-open", 1),
    ))
    .unwrap();
    block_on(storage.append(Sequence::ZERO, vec![started(1, "first")])).unwrap();
    let path = repository
        .session_path(&SessionId::new("corrupt-open"))
        .unwrap();
    let content = fs::read_to_string(&path).unwrap();
    let first_batch = content.lines().nth(1).unwrap();
    let second_batch = first_batch
        .replace("\"first\"", "\"second\"")
        .replace("\"sequence\":1", "\"sequence\":2")
        .replace("\"unix_millis\":1", "\"unix_millis\":2");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{second_batch}").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let state = FileSessionStorage::open(path)
        .unwrap()
        .load_state_sync()
        .unwrap();
    assert!(matches!(
        state.recovery_decision(&LaneName::new("main")),
        RecoveryDecision::Corrupt { open_operations } if open_operations.len() == 2
    ));
}

#[test]
fn session_operation_recovery_reconstructs_intent() {
    // §7.6 and §10.10 session_operation_recovery_reconstructs_intent.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    let storage = block_on(SessionRepository::create(
        &repository,
        request("recover", 1),
    ))
    .unwrap();
    block_on(storage.append(Sequence::ZERO, vec![started(1, "run")])).unwrap();
    drop(storage);

    let reopened = block_on(SessionRepository::open(
        &repository,
        &SessionId::new("recover"),
    ))
    .unwrap();
    let state = block_on(reopened.load_state()).unwrap();
    let RecoveryDecision::Resume { operation, .. } =
        state.recovery_decision(&LaneName::new("main"))
    else {
        panic!("open operation must be resumable after reopen");
    };
    assert_eq!(operation.run_id(), Some(RunId::new("run")));
    assert!(matches!(
        operation,
        OperationRecord::Started {
            intent: OperationIntent::Run { .. },
            ..
        }
    ));
}

#[test]
fn session_atomic_fork_rewrite_publishes_complete_destination() {
    // §7.5 and §7.13 protocol semantics. Pi basis: jsonl/storage.ts stages a
    // complete fork and atomically renames it over the destination.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    let source = block_on(SessionRepository::create(&repository, request("source", 1))).unwrap();
    block_on(source.append(
        Sequence::ZERO,
        vec![
            custom_entry(1, "root", None, Some("main")),
            custom_entry(2, "tail", Some("root"), Some("main")),
        ],
    ))
    .unwrap();
    let fork = block_on(SessionRepository::fork(
        &repository,
        &SessionId::new("source"),
        ForkRequest {
            session_id: SessionId::new("fork"),
            created_at: Timestamp::from_unix_millis(2),
            environment: SessionEnvironmentMetadata::default(),
            position: ForkPosition::WholeTree,
        },
    ))
    .unwrap();
    assert_eq!(
        block_on(fork.load_state()).unwrap().sequence(),
        Sequence::new(3)
    );
    assert_eq!(
        block_on(source.load_state()).unwrap().sequence(),
        Sequence::new(2)
    );
    let path = repository.session_path(&SessionId::new("fork")).unwrap();
    assert!(path.exists());
    assert!(!path.with_extension("agentprism-session.tmp").exists());
    assert_eq!(
        block_on(
            block_on(SessionRepository::open(
                &repository,
                &SessionId::new("fork")
            ))
            .unwrap()
            .load_state()
        )
        .unwrap()
        .sequence(),
        Sequence::new(3)
    );
}

#[test]
fn session_native_records_carry_schema_versions() {
    // Native-format choice under §7.13's owner ruling: both the immutable
    // header and every append batch independently carry schema_version.
    let directory = tempfile::tempdir().unwrap();
    let repository = FileSessionRepository::new(directory.path()).unwrap();
    let storage = block_on(SessionRepository::create(
        &repository,
        request("versioned", 1),
    ))
    .unwrap();
    block_on(storage.append(
        Sequence::ZERO,
        vec![custom_entry(1, "root", None, Some("main"))],
    ))
    .unwrap();
    let content = fs::read_to_string(
        repository
            .session_path(&SessionId::new("versioned"))
            .unwrap(),
    )
    .unwrap();
    let lines = content.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert!(
        lines
            .iter()
            .all(|line| line.contains("\"schema_version\":1"))
    );
    assert!(lines[0].contains("\"format\":\"agentprism-session\""));
}
