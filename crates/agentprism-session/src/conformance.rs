//! Reusable backend-generic session storage and recovery conformance.
//!
//! This module is exported with the `conformance` feature. The crate also
//! compiles it for its own tests so the in-memory and native file backends are
//! continuously certified by the same scenarios.

use crate::{
    AppendReceipt, CreateSessionRequest, EntryBase, EntryId, ForkPosition, ForkRequest, LaneName,
    LocalBoxFuture, LocalSessionRepository, LocalSessionStorage, OperationIntent, OperationRecord,
    OperationRecordBase, OperationRecordId, RecoveryDecision, Sequence, SessionEntry,
    SessionEnvironmentMetadata, SessionError, SessionErrorKind, SessionFact, SessionMetadata,
    SessionMutation, SessionQuery, SessionReducer, SessionRepository, SessionState, SessionStorage,
    TailRepairReport, UsageAttribution,
};
use agentprism_ai::{Cost, Currency, RunId, Timestamp, Usage, UsageSource};
use agentprism_core::AgentRecord;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{
    error::Error,
    fmt,
    future::{Future, poll_fn},
    rc::Rc,
    sync::{Arc, Barrier},
    task::{Poll, Wake, Waker},
    thread,
};

/// Schema of [`StorageConformanceReport`].
pub const STORAGE_CONFORMANCE_REPORT_SCHEMA_VERSION: u32 = 1;

/// The Architecture v2 part 2 §10.10 storage/recovery case names.
pub const STORAGE_RECOVERY_CONFORMANCE_CASES: &[&str] = &[
    "session_sequence_starts_at_one",
    "session_sequence_is_global_across_mutation_kinds",
    "session_sequence_gap_is_corruption",
    "session_entry_parent_must_exist",
    "session_lane_head_moves_on_append",
    "session_lane_can_move_to_ancestor",
    "session_multiple_lanes_share_entry_tree",
    "session_branch_scan_leaf_to_root",
    "session_global_entry_query_sequence_order",
    "session_fact_latest_value_wins",
    "session_label_is_global_not_branch_scoped",
    "session_stats_derive_from_usage_records",
    "session_open_operation_detected",
    "session_multiple_open_operations_is_corruption",
    "session_operation_recovery_reconstructs_intent",
    "session_reducer_replay_equals_live_state",
];

/// Complete backend contract mirrored from pinned Pi's reusable session suite.
///
/// The first sixteen names are the normative Architecture v2 part 2 §10.10
/// reducer/recovery cases. The remaining cases certify the format-agnostic
/// repository behaviors exercised by pinned `memory.test.ts`: metadata,
/// bounded reads, detached snapshots, append failure atomicity, concurrent
/// serialization, reopen, tail repair, forks, listing, and persistence
/// round-trips.
pub const SESSION_BACKEND_CONFORMANCE_CASES: &[&str] = &[
    "session_sequence_starts_at_one",
    "session_sequence_is_global_across_mutation_kinds",
    "session_sequence_gap_is_corruption",
    "session_entry_parent_must_exist",
    "session_lane_head_moves_on_append",
    "session_lane_can_move_to_ancestor",
    "session_multiple_lanes_share_entry_tree",
    "session_branch_scan_leaf_to_root",
    "session_global_entry_query_sequence_order",
    "session_fact_latest_value_wins",
    "session_label_is_global_not_branch_scoped",
    "session_stats_derive_from_usage_records",
    "session_open_operation_detected",
    "session_multiple_open_operations_is_corruption",
    "session_operation_recovery_reconstructs_intent",
    "session_reducer_replay_equals_live_state",
    "session_backend_metadata_round_trip",
    "session_backend_bounded_log_queries",
    "session_backend_read_snapshots_are_detached",
    "session_backend_failed_append_is_atomic",
    "session_backend_concurrent_append_is_serialized",
    "session_backend_repository_create_list_open",
    "session_backend_branch_fork_round_trip",
    "session_backend_tree_fork_round_trip",
    "session_backend_repair_tail_reports_current_state",
    "session_backend_persistence_round_trip",
];

type AppendOutcome = Result<AppendReceipt, SessionError>;
type ConcurrentAppendOutcomes = (AppendOutcome, AppendOutcome);

/// Successful reusable conformance result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StorageConformanceReport {
    /// Report schema.
    pub schema_version: u32,
    /// Cases completed in the normative §10.10 order.
    pub completed_cases: Vec<String>,
}

/// Failure of one named reusable storage conformance case.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StorageConformanceError {
    /// Exact §10.10 case name.
    pub case_name: String,
    /// Backend-neutral failure description.
    pub message: String,
}

impl fmt::Display for StorageConformanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.case_name, self.message)
    }
}

impl Error for StorageConformanceError {}

/// Runs the complete §10.10 storage/recovery suite against a Send repository.
///
/// `session_prefix` must be a repository-valid session-id prefix that is unique
/// within the supplied fixture.
pub async fn run_send_storage_conformance(
    repository: &dyn SessionRepository,
    session_prefix: &str,
) -> Result<StorageConformanceReport, StorageConformanceError> {
    run_storage_conformance(&SendRepositoryAdapter(repository), session_prefix).await
}

/// Runs the complete §10.10 storage/recovery suite against a Local repository.
///
/// `session_prefix` must be a repository-valid session-id prefix that is unique
/// within the supplied fixture.
pub async fn run_local_storage_conformance(
    repository: &dyn LocalSessionRepository,
    session_prefix: &str,
) -> Result<StorageConformanceReport, StorageConformanceError> {
    run_storage_conformance(&LocalRepositoryAdapter(repository), session_prefix).await
}

trait ConformanceRepository {
    fn create(
        &self,
        request: CreateSessionRequest,
    ) -> LocalBoxFuture<'_, Result<Box<dyn ConformanceStorage>, SessionError>>;

    fn open(
        &self,
        id: &crate::SessionId,
    ) -> LocalBoxFuture<'_, Result<Box<dyn ConformanceStorage>, SessionError>>;

    fn fork(
        &self,
        source: &crate::SessionId,
        request: ForkRequest,
    ) -> LocalBoxFuture<'_, Result<Box<dyn ConformanceStorage>, SessionError>>;

    fn list(
        &self,
        query: SessionQuery,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>>;

    fn concurrent_append(
        &self,
        id: &crate::SessionId,
        expected_sequence: Sequence,
        left: Vec<SessionMutation>,
        right: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<ConcurrentAppendOutcomes, SessionError>>;
}

trait ConformanceStorage {
    fn metadata(&self) -> LocalBoxFuture<'_, Result<SessionMetadata, SessionError>>;

    fn load_state(&self) -> LocalBoxFuture<'_, Result<SessionState, SessionError>>;

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<AppendReceipt, SessionError>>;

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>>;

    fn repair_tail(&self) -> LocalBoxFuture<'_, Result<TailRepairReport, SessionError>>;
}

struct SendRepositoryAdapter<'a>(&'a dyn SessionRepository);

impl ConformanceRepository for SendRepositoryAdapter<'_> {
    fn create(
        &self,
        request: CreateSessionRequest,
    ) -> LocalBoxFuture<'_, Result<Box<dyn ConformanceStorage>, SessionError>> {
        Box::pin(async move {
            let storage = self.0.create(request).await?;
            Ok(Box::new(SendStorageAdapter(storage)) as Box<dyn ConformanceStorage>)
        })
    }

    fn open(
        &self,
        id: &crate::SessionId,
    ) -> LocalBoxFuture<'_, Result<Box<dyn ConformanceStorage>, SessionError>> {
        let id = id.clone();
        Box::pin(async move {
            let storage = self.0.open(&id).await?;
            Ok(Box::new(SendStorageAdapter(storage)) as Box<dyn ConformanceStorage>)
        })
    }

    fn fork(
        &self,
        source: &crate::SessionId,
        request: ForkRequest,
    ) -> LocalBoxFuture<'_, Result<Box<dyn ConformanceStorage>, SessionError>> {
        let source = source.clone();
        Box::pin(async move {
            let storage = self.0.fork(&source, request).await?;
            Ok(Box::new(SendStorageAdapter(storage)) as Box<dyn ConformanceStorage>)
        })
    }

    fn list(
        &self,
        query: SessionQuery,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        Box::pin(async move { self.0.list(query).await })
    }

    fn concurrent_append(
        &self,
        id: &crate::SessionId,
        expected_sequence: Sequence,
        left: Vec<SessionMutation>,
        right: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<ConcurrentAppendOutcomes, SessionError>> {
        let id = id.clone();
        Box::pin(async move {
            let left_storage = self.0.open(&id).await?;
            let right_storage = self.0.open(&id).await?;
            let barrier = Arc::new(Barrier::new(3));
            let left_barrier = barrier.clone();
            let left_thread = thread::spawn(move || {
                left_barrier.wait();
                block_on_send(left_storage.append(expected_sequence, left))
            });
            let right_barrier = barrier.clone();
            let right_thread = thread::spawn(move || {
                right_barrier.wait();
                block_on_send(right_storage.append(expected_sequence, right))
            });
            barrier.wait();
            Ok((
                left_thread.join().unwrap_or_else(|_| {
                    Err(SessionError::new(
                        SessionErrorKind::Storage,
                        "left concurrent append thread panicked",
                    ))
                }),
                right_thread.join().unwrap_or_else(|_| {
                    Err(SessionError::new(
                        SessionErrorKind::Storage,
                        "right concurrent append thread panicked",
                    ))
                }),
            ))
        })
    }
}

struct LocalRepositoryAdapter<'a>(&'a dyn LocalSessionRepository);

impl ConformanceRepository for LocalRepositoryAdapter<'_> {
    fn create(
        &self,
        request: CreateSessionRequest,
    ) -> LocalBoxFuture<'_, Result<Box<dyn ConformanceStorage>, SessionError>> {
        Box::pin(async move {
            let storage = self.0.create(request).await?;
            Ok(Box::new(LocalStorageAdapter(storage)) as Box<dyn ConformanceStorage>)
        })
    }

    fn open(
        &self,
        id: &crate::SessionId,
    ) -> LocalBoxFuture<'_, Result<Box<dyn ConformanceStorage>, SessionError>> {
        let id = id.clone();
        Box::pin(async move {
            let storage = self.0.open(&id).await?;
            Ok(Box::new(LocalStorageAdapter(storage)) as Box<dyn ConformanceStorage>)
        })
    }

    fn fork(
        &self,
        source: &crate::SessionId,
        request: ForkRequest,
    ) -> LocalBoxFuture<'_, Result<Box<dyn ConformanceStorage>, SessionError>> {
        let source = source.clone();
        Box::pin(async move {
            let storage = self.0.fork(&source, request).await?;
            Ok(Box::new(LocalStorageAdapter(storage)) as Box<dyn ConformanceStorage>)
        })
    }

    fn list(
        &self,
        query: SessionQuery,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        self.0.list(query)
    }

    fn concurrent_append(
        &self,
        id: &crate::SessionId,
        expected_sequence: Sequence,
        left: Vec<SessionMutation>,
        right: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<ConcurrentAppendOutcomes, SessionError>> {
        let id = id.clone();
        Box::pin(async move {
            let left_storage = self.0.open(&id).await?;
            let right_storage = self.0.open(&id).await?;
            let mut left = left_storage.append(expected_sequence, left);
            let mut right = right_storage.append(expected_sequence, right);
            let mut left_result = None;
            let mut right_result = None;
            Ok(poll_fn(|context| {
                if left_result.is_none()
                    && let Poll::Ready(result) = left.as_mut().poll(context)
                {
                    left_result = Some(result);
                }
                if right_result.is_none()
                    && let Poll::Ready(result) = right.as_mut().poll(context)
                {
                    right_result = Some(result);
                }
                match (left_result.take(), right_result.take()) {
                    (Some(left), Some(right)) => Poll::Ready((left, right)),
                    (left, right) => {
                        left_result = left;
                        right_result = right;
                        Poll::Pending
                    }
                }
            })
            .await)
        })
    }
}

struct SendStorageAdapter(Arc<dyn SessionStorage>);

impl ConformanceStorage for SendStorageAdapter {
    fn metadata(&self) -> LocalBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        Box::pin(async move { self.0.metadata().await })
    }

    fn load_state(&self) -> LocalBoxFuture<'_, Result<SessionState, SessionError>> {
        Box::pin(async move { self.0.load_state().await })
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        Box::pin(async move { self.0.append(expected_sequence, mutations).await })
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        Box::pin(async move { self.0.log(after, limit).await })
    }

    fn repair_tail(&self) -> LocalBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        Box::pin(async move { self.0.repair_tail().await })
    }
}

struct LocalStorageAdapter(Rc<dyn LocalSessionStorage>);

impl ConformanceStorage for LocalStorageAdapter {
    fn metadata(&self) -> LocalBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        self.0.metadata()
    }

    fn load_state(&self) -> LocalBoxFuture<'_, Result<SessionState, SessionError>> {
        self.0.load_state()
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        self.0.append(expected_sequence, mutations)
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        self.0.log(after, limit)
    }

    fn repair_tail(&self) -> LocalBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        self.0.repair_tail()
    }
}

async fn run_storage_conformance(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<StorageConformanceReport, StorageConformanceError> {
    let mut completed = Vec::new();
    macro_rules! run_case {
        ($name:literal, $case:ident) => {{
            $case(repository, prefix)
                .await
                .map_err(|message| StorageConformanceError {
                    case_name: $name.to_owned(),
                    message,
                })?;
            completed.push($name.to_owned());
        }};
    }

    run_case!("session_sequence_starts_at_one", sequence_starts_at_one);
    run_case!(
        "session_sequence_is_global_across_mutation_kinds",
        sequence_is_global
    );
    run_case!(
        "session_sequence_gap_is_corruption",
        sequence_gap_is_corruption
    );
    run_case!("session_entry_parent_must_exist", entry_parent_must_exist);
    run_case!(
        "session_lane_head_moves_on_append",
        lane_head_moves_on_append
    );
    run_case!(
        "session_lane_can_move_to_ancestor",
        lane_can_move_to_ancestor
    );
    run_case!(
        "session_multiple_lanes_share_entry_tree",
        multiple_lanes_share_tree
    );
    run_case!("session_branch_scan_leaf_to_root", branch_scan_leaf_to_root);
    run_case!(
        "session_global_entry_query_sequence_order",
        global_entry_sequence_order
    );
    run_case!("session_fact_latest_value_wins", fact_latest_value_wins);
    run_case!("session_label_is_global_not_branch_scoped", label_is_global);
    run_case!(
        "session_stats_derive_from_usage_records",
        stats_derive_from_usage
    );
    run_case!("session_open_operation_detected", open_operation_detected);
    run_case!(
        "session_multiple_open_operations_is_corruption",
        multiple_open_operations_is_corruption
    );
    run_case!(
        "session_operation_recovery_reconstructs_intent",
        operation_recovery_reconstructs_intent
    );
    run_case!(
        "session_reducer_replay_equals_live_state",
        reducer_replay_equals_live
    );
    run_case!(
        "session_backend_metadata_round_trip",
        backend_metadata_round_trip
    );
    run_case!(
        "session_backend_bounded_log_queries",
        backend_bounded_log_queries
    );
    run_case!(
        "session_backend_read_snapshots_are_detached",
        backend_read_snapshots_are_detached
    );
    run_case!(
        "session_backend_failed_append_is_atomic",
        backend_failed_append_is_atomic
    );
    run_case!(
        "session_backend_concurrent_append_is_serialized",
        backend_concurrent_append_is_serialized
    );
    run_case!(
        "session_backend_repository_create_list_open",
        backend_repository_create_list_open
    );
    run_case!(
        "session_backend_branch_fork_round_trip",
        backend_branch_fork_round_trip
    );
    run_case!(
        "session_backend_tree_fork_round_trip",
        backend_tree_fork_round_trip
    );
    run_case!(
        "session_backend_repair_tail_reports_current_state",
        backend_repair_tail_reports_current_state
    );
    run_case!(
        "session_backend_persistence_round_trip",
        backend_persistence_round_trip
    );

    Ok(StorageConformanceReport {
        schema_version: STORAGE_CONFORMANCE_REPORT_SCHEMA_VERSION,
        completed_cases: completed,
    })
}

async fn create_storage(
    repository: &dyn ConformanceRepository,
    prefix: &str,
    suffix: &str,
) -> Result<Box<dyn ConformanceStorage>, String> {
    repository
        .create(CreateSessionRequest::new(
            format!("{prefix}-{suffix}"),
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .await
        .map_err(|error| error.to_string())
}

async fn sequence_starts_at_one(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "sequence-start").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![custom_entry(1, "root", None, Some("main"))],
        )
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        storage
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            .sequence()
            == Sequence::FIRST,
        "first accepted mutation was not sequence one",
    )
}

async fn sequence_is_global(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "global-sequence").await?;
    let mutations = vec![
        custom_entry(1, "root", None, Some("main")),
        SessionMutation::Lane {
            sequence: Sequence::new(2),
            lane: LaneName::new("thread"),
            leaf_id: Some(EntryId::new("root")),
        },
        SessionMutation::Fact {
            sequence: Sequence::new(3),
            fact: SessionFact::Name {
                name: Some("name".to_owned()),
            },
        },
        operation_started(4, "run", "thread"),
    ];
    storage
        .append(Sequence::ZERO, mutations)
        .await
        .map_err(|error| error.to_string())?;
    let sequences = storage
        .log(None, None)
        .await
        .map_err(|error| error.to_string())?
        .iter()
        .map(SessionMutation::sequence)
        .collect::<Vec<_>>();
    ensure(
        sequences == (1..=4).map(Sequence::new).collect::<Vec<_>>(),
        "mutation kinds did not share one global sequence",
    )
}

async fn sequence_gap_is_corruption(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "sequence-gap").await?;
    let error = storage
        .append(
            Sequence::ZERO,
            vec![custom_entry(2, "gap", None, Some("main"))],
        )
        .await
        .expect_err("sequence gap must fail");
    ensure(
        error.kind == SessionErrorKind::Corruption,
        "sequence gap was not classified as corruption",
    )?;
    ensure(
        storage
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            .sequence()
            == Sequence::ZERO,
        "failed append changed storage state",
    )
}

async fn entry_parent_must_exist(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "missing-parent").await?;
    let error = storage
        .append(
            Sequence::ZERO,
            vec![custom_entry(1, "child", Some("missing"), None)],
        )
        .await
        .expect_err("missing parent must fail");
    ensure(
        error.kind == SessionErrorKind::Corruption,
        "missing parent was not classified as corruption",
    )
}

async fn lane_head_moves_on_append(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "lane-append").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![custom_entry(1, "root", None, Some("main"))],
        )
        .await
        .map_err(|error| error.to_string())?;
    let state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        state.lane_leaf(&LaneName::new("main")) == Some(&Some(EntryId::new("root"))),
        "lane did not advance to appended entry",
    )
}

async fn lane_can_move_to_ancestor(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "lane-ancestor").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                custom_entry(1, "root", None, Some("main")),
                custom_entry(2, "child", Some("root"), Some("main")),
                SessionMutation::Lane {
                    sequence: Sequence::new(3),
                    lane: LaneName::new("main"),
                    leaf_id: Some(EntryId::new("root")),
                },
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    let state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        state.lane_leaf(&LaneName::new("main")) == Some(&Some(EntryId::new("root"))),
        "lane did not move to an ancestor",
    )
}

async fn multiple_lanes_share_tree(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "shared-tree").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                custom_entry(1, "root", None, Some("main")),
                SessionMutation::Lane {
                    sequence: Sequence::new(2),
                    lane: LaneName::new("thread"),
                    leaf_id: Some(EntryId::new("root")),
                },
                custom_entry(3, "main-child", Some("root"), Some("main")),
                custom_entry(4, "thread-child", Some("root"), Some("thread")),
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    let state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        state.lane_leaf(&LaneName::new("main")) == Some(&Some(EntryId::new("main-child")))
            && state.lane_leaf(&LaneName::new("thread"))
                == Some(&Some(EntryId::new("thread-child"))),
        "lanes did not retain independent leaves in the shared tree",
    )
}

async fn branch_scan_leaf_to_root(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "branch-scan").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                custom_entry(1, "root", None, Some("main")),
                custom_entry(2, "child", Some("root"), Some("main")),
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    let state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    let ids = state
        .scan_branch_leaf_to_root(&EntryId::new("child"))
        .map_err(|error| error.to_string())?
        .iter()
        .map(|entry| entry.id().as_str())
        .collect::<Vec<_>>();
    ensure(
        ids == ["child", "root"],
        "branch scan order was not leaf to root",
    )
}

async fn global_entry_sequence_order(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "entry-order").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                custom_entry(1, "root", None, Some("main")),
                custom_entry(2, "tail", Some("root"), Some("main")),
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    let state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    let ids = state
        .entries_in_sequence_order()
        .iter()
        .map(|entry| entry.id().as_str())
        .collect::<Vec<_>>();
    ensure(
        ids == ["root", "tail"],
        "global entry query was not in sequence order",
    )
}

async fn fact_latest_value_wins(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "latest-fact").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                SessionMutation::Fact {
                    sequence: Sequence::new(1),
                    fact: SessionFact::Name {
                        name: Some("first".to_owned()),
                    },
                },
                SessionMutation::Fact {
                    sequence: Sequence::new(2),
                    fact: SessionFact::Name {
                        name: Some("second".to_owned()),
                    },
                },
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        storage
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            .name()
            == Some("second"),
        "latest name fact did not win",
    )
}

async fn label_is_global(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "global-label").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                custom_entry(1, "root", None, Some("main")),
                SessionMutation::Lane {
                    sequence: Sequence::new(2),
                    lane: LaneName::new("thread"),
                    leaf_id: Some(EntryId::new("root")),
                },
                SessionMutation::Fact {
                    sequence: Sequence::new(3),
                    fact: SessionFact::Label {
                        target_id: EntryId::new("root"),
                        label: Some("shared".to_owned()),
                    },
                },
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        storage
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            .label(&EntryId::new("root"))
            == Some("shared"),
        "label was not retained as a global fact",
    )
}

async fn stats_derive_from_usage(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "stats").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![message_entry(1, "message"), usage_record(2, "usage")],
        )
        .await
        .map_err(|error| error.to_string())?;
    let state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    let stats = state.stats();
    ensure(
        stats.message_count == 1
            && stats.cached_tokens == 3
            && stats.uncached_tokens == 12
            && stats.total_tokens == 20
            && stats.cost_micros_by_currency.get("USD") == Some(&50),
        "statistics were not derived from durable message and usage records",
    )
}

async fn open_operation_detected(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "open-operation").await?;
    storage
        .append(Sequence::ZERO, vec![operation_started(1, "run", "main")])
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        matches!(
            storage
                .load_state()
                .await
                .map_err(|error| error.to_string())?
                .recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Resume { .. }
        ),
        "one open operation was not resumable",
    )?;
    let error = storage
        .append(
            Sequence::FIRST,
            vec![operation_started(2, "second", "main")],
        )
        .await
        .expect_err("a second live operation start must be rejected");
    ensure(
        error.kind == SessionErrorKind::Storage,
        "a second live operation start was not a storage error",
    )?;
    ensure(
        storage
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            .open_operations(&LaneName::new("main"))
            .len()
            == 1,
        "failed second operation start changed backend state",
    )
}

async fn multiple_open_operations_is_corruption(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "multiple-open").await?;
    storage
        .append(Sequence::ZERO, vec![operation_started(1, "first", "main")])
        .await
        .map_err(|error| error.to_string())?;
    let error = storage
        .append(
            Sequence::FIRST,
            vec![operation_started(2, "second", "main")],
        )
        .await
        .expect_err("live backend must reject a second unresolved operation");
    ensure(
        error.kind == SessionErrorKind::Storage,
        "live second-start rejection had the wrong error kind",
    )?;
    ensure(
        storage
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            .open_operations(&LaneName::new("main"))
            .len()
            == 1,
        "live second-start rejection changed persisted open operations",
    )?;

    // Replay is intentionally more permissive than live append so a backend
    // importing a corrupt log retains every unresolved start for diagnosis.
    let state = SessionState::replay([
        operation_started(1, "first", "main"),
        operation_started(2, "second", "main"),
    ])
    .map_err(|error| error.to_string())?;
    ensure(
        matches!(
            state.recovery_decision(&LaneName::new("main")),
            RecoveryDecision::Corrupt { ref open_operations } if open_operations.len() == 2
        ),
        "replay did not preserve multiple unresolved starts for diagnosis",
    )
}

async fn operation_recovery_reconstructs_intent(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let id = crate::SessionId::new(format!("{prefix}-recovery-intent"));
    let storage = repository
        .create(CreateSessionRequest::new(
            id.clone(),
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                operation_started(1, "run", "main"),
                SessionMutation::Record {
                    record: OperationRecord::StepAttempt {
                        base: record_base(2, "attempt", "main"),
                        run_id: RunId::new("run"),
                        step: crate::OperationStep::Assistant,
                        attempt: 1,
                        result_entry_id: EntryId::new("assistant-result"),
                        compaction_reason: None,
                    },
                },
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    drop(storage);
    let reopened = repository
        .open(&id)
        .await
        .map_err(|error| error.to_string())?;
    let decision = reopened
        .load_state()
        .await
        .map_err(|error| error.to_string())?
        .recovery_decision(&LaneName::new("main"));
    let RecoveryDecision::Resume {
        operation,
        completed_steps,
    } = decision
    else {
        return Err("durable operation was not resumable".to_owned());
    };
    ensure(
        matches!(
            operation,
            OperationRecord::Started {
                intent: OperationIntent::Run { .. },
                ..
            }
        ) && completed_steps.len() == 1,
        "recovery did not reconstruct intent and completed steps",
    )
}

async fn reducer_replay_equals_live(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "replay-live").await?;
    let mutations = vec![
        custom_entry(1, "root", None, Some("main")),
        SessionMutation::Fact {
            sequence: Sequence::new(2),
            fact: SessionFact::Name {
                name: Some("example".to_owned()),
            },
        },
        usage_record(3, "usage"),
    ];
    storage
        .append(Sequence::ZERO, mutations.clone())
        .await
        .map_err(|error| error.to_string())?;
    let live = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    let replayed = SessionState::replay(mutations).map_err(|error| error.to_string())?;
    ensure(live == replayed, "reducer replay did not equal live state")
}

async fn backend_metadata_round_trip(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let id = crate::SessionId::new(format!("{prefix}-metadata"));
    let parent = crate::SessionId::new(format!("{prefix}-metadata-parent"));
    let environment = SessionEnvironmentMetadata {
        working_directory: Some("/conformance/workspace".to_owned()),
        ..SessionEnvironmentMetadata::default()
    };
    let mut request = CreateSessionRequest::new(
        id.clone(),
        Timestamp::from_unix_millis(17),
        environment.clone(),
    );
    request.parent_session_id = Some(parent.clone());
    let storage = repository
        .create(request)
        .await
        .map_err(|error| error.to_string())?;
    let initial = storage
        .metadata()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        initial.schema_version == crate::SESSION_METADATA_SCHEMA_VERSION
            && initial.session_id == id
            && initial.created_at == Timestamp::from_unix_millis(17)
            && initial.parent_session_id == Some(parent)
            && initial.environment == environment
            && initial.last_sequence == Sequence::ZERO,
        "created metadata did not preserve the native header",
    )?;
    storage
        .append(
            Sequence::ZERO,
            vec![custom_entry(1, "metadata-entry", None, Some("main"))],
        )
        .await
        .map_err(|error| error.to_string())?;
    drop(storage);
    let reopened = repository
        .open(&id)
        .await
        .map_err(|error| error.to_string())?;
    let reopened_metadata = reopened
        .metadata()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        reopened_metadata.last_sequence == Sequence::FIRST && reopened_metadata.session_id == id,
        "metadata did not survive repository reopen",
    )
}

async fn backend_bounded_log_queries(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "bounded-log").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                custom_entry(1, "root", None, Some("main")),
                SessionMutation::Lane {
                    sequence: Sequence::new(2),
                    lane: LaneName::new("thread"),
                    leaf_id: Some(EntryId::new("root")),
                },
                SessionMutation::Fact {
                    sequence: Sequence::new(3),
                    fact: SessionFact::Name {
                        name: Some("bounded".to_owned()),
                    },
                },
                operation_started(4, "bounded-run", "thread"),
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    let bounded = storage
        .log(Some(Sequence::FIRST), Some(2))
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        bounded
            .iter()
            .map(SessionMutation::sequence)
            .collect::<Vec<_>>()
            == [Sequence::new(2), Sequence::new(3)],
        "exclusive cursor or bounded log order was incorrect",
    )?;
    let error = storage
        .log(None, Some(0))
        .await
        .expect_err("zero query limit must fail");
    ensure(
        error.kind == SessionErrorKind::InvalidQuery,
        "zero query limit was not classified as invalid",
    )?;
    let state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        state.entries_in_sequence_order().len() == 1
            && state.records_in_sequence_order().len() == 1
            && state.name() == Some("bounded"),
        "format-agnostic state queries did not expose the committed mutation kinds",
    )
}

async fn backend_read_snapshots_are_detached(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "detached-reads").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                custom_entry(1, "root", None, Some("main")),
                SessionMutation::Fact {
                    sequence: Sequence::new(2),
                    fact: SessionFact::Name {
                        name: Some("durable".to_owned()),
                    },
                },
            ],
        )
        .await
        .map_err(|error| error.to_string())?;

    let mut detached_state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    detached_state
        .apply(&SessionMutation::Fact {
            sequence: Sequence::new(3),
            fact: SessionFact::Name {
                name: Some("mutated-copy".to_owned()),
            },
        })
        .map_err(|error| error.to_string())?;
    let mut detached_log = storage
        .log(None, None)
        .await
        .map_err(|error| error.to_string())?;
    detached_log.clear();
    let mut detached_metadata = storage
        .metadata()
        .await
        .map_err(|error| error.to_string())?;
    detached_metadata.session_id = crate::SessionId::new("changed-copy");

    let durable_state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    let durable_log = storage
        .log(None, None)
        .await
        .map_err(|error| error.to_string())?;
    let durable_metadata = storage
        .metadata()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        durable_state.sequence() == Sequence::new(2)
            && durable_state.name() == Some("durable")
            && durable_log.len() == 2
            && durable_metadata
                .session_id
                .as_str()
                .ends_with("detached-reads"),
        "mutating detached read values changed backend state",
    )
}

async fn backend_failed_append_is_atomic(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let storage = create_storage(repository, prefix, "failed-append").await?;
    storage
        .append(
            Sequence::ZERO,
            vec![custom_entry(1, "root", None, Some("main"))],
        )
        .await
        .map_err(|error| error.to_string())?;
    let before = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    let error = storage
        .append(
            Sequence::FIRST,
            vec![
                SessionMutation::Fact {
                    sequence: Sequence::new(2),
                    fact: SessionFact::Name {
                        name: Some("must-not-commit".to_owned()),
                    },
                },
                custom_entry(3, "orphan", Some("missing"), None),
            ],
        )
        .await
        .expect_err("a reduction failure must reject the whole append batch");
    ensure(
        error.kind == SessionErrorKind::Corruption,
        "failed batch had the wrong error category",
    )?;
    let after = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        before == after,
        "failed batch partially changed backend state",
    )?;
    storage
        .append(
            Sequence::FIRST,
            vec![SessionMutation::Fact {
                sequence: Sequence::new(2),
                fact: SessionFact::Name {
                    name: Some("accepted".to_owned()),
                },
            }],
        )
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        storage
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            .name()
            == Some("accepted"),
        "backend could not append at the original sequence after failure",
    )
}

async fn backend_concurrent_append_is_serialized(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let id = crate::SessionId::new(format!("{prefix}-concurrent-append"));
    repository
        .create(CreateSessionRequest::new(
            id.clone(),
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    let (left, right) = repository
        .concurrent_append(
            &id,
            Sequence::ZERO,
            vec![custom_entry(1, "left", None, Some("main"))],
            vec![custom_entry(1, "right", None, Some("main"))],
        )
        .await
        .map_err(|error| error.to_string())?;
    let outcomes = [&left, &right];
    ensure(
        outcomes.iter().filter(|result| result.is_ok()).count() == 1,
        "concurrent optimistic appends did not have exactly one winner",
    )?;
    ensure(
        outcomes
            .iter()
            .filter(|result| {
                result
                    .as_ref()
                    .is_err_and(|error| error.kind == SessionErrorKind::SequenceConflict)
            })
            .count()
            == 1,
        "concurrent optimistic appends did not return one sequence conflict",
    )?;
    let storage = repository
        .open(&id)
        .await
        .map_err(|error| error.to_string())?;
    let state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        state.sequence() == Sequence::FIRST
            && state.entries_in_sequence_order().len() == 1
            && storage
                .log(None, None)
                .await
                .map_err(|error| error.to_string())?
                .len()
                == 1,
        "concurrent append was not serialized to one durable mutation",
    )
}

async fn backend_repository_create_list_open(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let parent = crate::SessionId::new(format!("{prefix}-list-parent"));
    let first_id = crate::SessionId::new(format!("{prefix}-list-first"));
    let second_id = crate::SessionId::new(format!("{prefix}-list-second"));
    for (id, created_at) in [(first_id.clone(), 10), (second_id.clone(), 20)] {
        let mut request = CreateSessionRequest::new(
            id,
            Timestamp::from_unix_millis(created_at),
            SessionEnvironmentMetadata::default(),
        );
        request.parent_session_id = Some(parent.clone());
        repository
            .create(request)
            .await
            .map_err(|error| error.to_string())?;
    }
    let duplicate = match repository
        .create(CreateSessionRequest::new(
            first_id.clone(),
            Timestamp::from_unix_millis(30),
            SessionEnvironmentMetadata::default(),
        ))
        .await
    {
        Ok(_) => return Err("duplicate repository create unexpectedly succeeded".to_owned()),
        Err(error) => error,
    };
    ensure(
        duplicate.kind == SessionErrorKind::AlreadyExists,
        "duplicate create had the wrong error kind",
    )?;
    let listed = repository
        .list(SessionQuery {
            parent_session_id: Some(parent),
            limit: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        listed
            .iter()
            .map(|metadata| metadata.session_id.clone())
            .collect::<Vec<_>>()
            == [second_id.clone(), first_id.clone()],
        "repository listing did not filter and sort metadata",
    )?;
    let limited = repository
        .list(SessionQuery {
            parent_session_id: listed[0].parent_session_id.clone(),
            limit: Some(1),
        })
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        limited.len() == 1 && limited[0].session_id == second_id,
        "repository listing did not apply its positive limit",
    )?;
    ensure(
        repository
            .list(SessionQuery {
                parent_session_id: None,
                limit: Some(0),
            })
            .await
            .expect_err("zero repository limit must fail")
            .kind
            == SessionErrorKind::InvalidQuery,
        "zero repository limit had the wrong error kind",
    )?;
    let opened = repository
        .open(&first_id)
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        opened
            .metadata()
            .await
            .map_err(|error| error.to_string())?
            .session_id
            == first_id,
        "repository open returned the wrong session",
    )?;
    let missing = crate::SessionId::new(format!("{prefix}-list-missing"));
    let missing_error = match repository.open(&missing).await {
        Ok(_) => return Err("opening a missing session unexpectedly succeeded".to_owned()),
        Err(error) => error,
    };
    ensure(
        missing_error.kind == SessionErrorKind::NotFound,
        "missing open had the wrong error kind",
    )
}

async fn backend_branch_fork_round_trip(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let source_id = crate::SessionId::new(format!("{prefix}-branch-source"));
    let source = repository
        .create(CreateSessionRequest::new(
            source_id.clone(),
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    source
        .append(
            Sequence::ZERO,
            vec![
                message_entry_on_lane(1, "root", None, Some("main")),
                message_entry_on_lane(2, "tail", Some("root"), Some("main")),
                operation_started(3, "source-run", "main"),
                SessionMutation::Fact {
                    sequence: Sequence::new(4),
                    fact: SessionFact::Name {
                        name: Some("source name".to_owned()),
                    },
                },
                SessionMutation::Fact {
                    sequence: Sequence::new(5),
                    fact: SessionFact::Label {
                        target_id: EntryId::new("tail"),
                        label: Some("copied".to_owned()),
                    },
                },
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    let fork_id = crate::SessionId::new(format!("{prefix}-branch-fork"));
    let fork = repository
        .fork(
            &source_id,
            ForkRequest {
                session_id: fork_id.clone(),
                created_at: Timestamp::from_unix_millis(2),
                environment: SessionEnvironmentMetadata::default(),
                position: ForkPosition::At(EntryId::new("tail")),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let fork_state = fork.load_state().await.map_err(|error| error.to_string())?;
    ensure(
        fork_state
            .entries_in_sequence_order()
            .iter()
            .map(|entry| entry.id().as_str())
            .collect::<Vec<_>>()
            == ["root", "tail"]
            && fork_state.records_in_sequence_order().is_empty()
            && fork_state.name() == Some("source name")
            && fork_state.label(&EntryId::new("tail")) == Some("copied"),
        "branch fork did not copy its prefix and selected facts without records",
    )?;
    ensure(
        fork.metadata()
            .await
            .map_err(|error| error.to_string())?
            .parent_session_id
            == Some(source_id.clone()),
        "branch fork metadata did not record its parent",
    )?;
    drop(fork);
    ensure(
        repository
            .open(&fork_id)
            .await
            .map_err(|error| error.to_string())?
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            == fork_state,
        "branch fork was not durably reopenable",
    )?;

    source
        .append(
            Sequence::new(5),
            vec![custom_entry(6, "custom-leaf", Some("tail"), Some("main"))],
        )
        .await
        .map_err(|error| error.to_string())?;
    let invalid_id = crate::SessionId::new(format!("{prefix}-invalid-fork"));
    let invalid = match repository
        .fork(
            &source_id,
            ForkRequest {
                session_id: invalid_id.clone(),
                created_at: Timestamp::from_unix_millis(3),
                environment: SessionEnvironmentMetadata::default(),
                position: ForkPosition::At(EntryId::new("custom-leaf")),
            },
        )
        .await
    {
        Ok(_) => return Err("non-message fork target unexpectedly succeeded".to_owned()),
        Err(error) => error,
    };
    ensure(
        invalid.kind == SessionErrorKind::InvalidForkTarget,
        "invalid fork target had the wrong error kind",
    )?;
    let absent_destination = match repository.open(&invalid_id).await {
        Ok(_) => return Err("failed fork destination was published".to_owned()),
        Err(error) => error,
    };
    ensure(
        absent_destination.kind == SessionErrorKind::NotFound,
        "failed fork published a destination session",
    )
}

async fn backend_tree_fork_round_trip(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let source_id = crate::SessionId::new(format!("{prefix}-tree-source"));
    let source = repository
        .create(CreateSessionRequest::new(
            source_id.clone(),
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    source
        .append(
            Sequence::ZERO,
            vec![
                message_entry_on_lane(1, "root", None, Some("main")),
                SessionMutation::Lane {
                    sequence: Sequence::new(2),
                    lane: LaneName::new("thread"),
                    leaf_id: Some(EntryId::new("root")),
                },
                message_entry_on_lane(3, "main-child", Some("root"), Some("main")),
                message_entry_on_lane(4, "thread-child", Some("root"), Some("thread")),
                SessionMutation::Fact {
                    sequence: Sequence::new(5),
                    fact: SessionFact::Name {
                        name: Some("tree source".to_owned()),
                    },
                },
                SessionMutation::Fact {
                    sequence: Sequence::new(6),
                    fact: SessionFact::Label {
                        target_id: EntryId::new("thread-child"),
                        label: Some("thread tip".to_owned()),
                    },
                },
                operation_started(7, "tree-run", "main"),
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    let fork_id = crate::SessionId::new(format!("{prefix}-tree-fork"));
    let fork = repository
        .fork(
            &source_id,
            ForkRequest {
                session_id: fork_id.clone(),
                created_at: Timestamp::from_unix_millis(2),
                environment: SessionEnvironmentMetadata::default(),
                position: ForkPosition::WholeTree,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let state = fork.load_state().await.map_err(|error| error.to_string())?;
    ensure(
        state
            .entries_in_sequence_order()
            .iter()
            .map(|entry| entry.id().as_str())
            .collect::<Vec<_>>()
            == ["root", "main-child", "thread-child"]
            && state.lanes()
                == [
                    crate::LaneState {
                        name: LaneName::new("main"),
                        leaf_id: Some(EntryId::new("main-child")),
                    },
                    crate::LaneState {
                        name: LaneName::new("thread"),
                        leaf_id: Some(EntryId::new("thread-child")),
                    },
                ]
            && state.records_in_sequence_order().is_empty()
            && state.name() == Some("tree source")
            && state.label(&EntryId::new("thread-child")) == Some("thread tip"),
        "tree fork did not preserve entries, lanes, and facts without records",
    )?;
    drop(fork);
    ensure(
        repository
            .open(&fork_id)
            .await
            .map_err(|error| error.to_string())?
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            == state,
        "tree fork did not survive repository reopen",
    )
}

async fn backend_repair_tail_reports_current_state(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let id = crate::SessionId::new(format!("{prefix}-repair-tail"));
    let storage = repository
        .create(CreateSessionRequest::new(
            id.clone(),
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    storage
        .append(
            Sequence::ZERO,
            vec![custom_entry(1, "root", None, Some("main"))],
        )
        .await
        .map_err(|error| error.to_string())?;
    let report = storage
        .repair_tail()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        report.schema_version == crate::TAIL_REPAIR_REPORT_SCHEMA_VERSION
            && !report.repaired
            && report.removed_bytes == 0
            && report.last_sequence == Sequence::FIRST,
        "healthy tail repair report did not match current state",
    )?;
    drop(storage);
    let reopened = repository
        .open(&id)
        .await
        .map_err(|error| error.to_string())?;
    let second = reopened
        .repair_tail()
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        second == report
            && reopened
                .load_state()
                .await
                .map_err(|error| error.to_string())?
                .sequence()
                == Sequence::FIRST,
        "tail repair was not idempotent across reopen",
    )
}

async fn backend_persistence_round_trip(
    repository: &dyn ConformanceRepository,
    prefix: &str,
) -> Result<(), String> {
    let id = crate::SessionId::new(format!("{prefix}-round-trip"));
    let storage = repository
        .create(CreateSessionRequest::new(
            id.clone(),
            Timestamp::from_unix_millis(1),
            SessionEnvironmentMetadata::default(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    storage
        .append(
            Sequence::ZERO,
            vec![
                message_entry_on_lane(1, "root", None, Some("main")),
                SessionMutation::Lane {
                    sequence: Sequence::new(2),
                    lane: LaneName::new("thread"),
                    leaf_id: Some(EntryId::new("root")),
                },
                custom_entry(3, "thread-note", Some("root"), Some("thread")),
                SessionMutation::Fact {
                    sequence: Sequence::new(4),
                    fact: SessionFact::Name {
                        name: Some("round trip".to_owned()),
                    },
                },
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    let state = storage
        .load_state()
        .await
        .map_err(|error| error.to_string())?;
    let log = storage
        .log(None, None)
        .await
        .map_err(|error| error.to_string())?;
    let encoded = serde_json::to_vec(&log).map_err(|error| error.to_string())?;
    let decoded: Vec<SessionMutation> =
        serde_json::from_slice(&encoded).map_err(|error| error.to_string())?;
    let replayed = SessionState::replay(decoded).map_err(|error| error.to_string())?;
    ensure(
        replayed == state,
        "serialized mutation round-trip changed reducer state",
    )?;
    drop(storage);
    let reopened = repository
        .open(&id)
        .await
        .map_err(|error| error.to_string())?;
    ensure(
        reopened
            .load_state()
            .await
            .map_err(|error| error.to_string())?
            == state
            && reopened
                .log(None, None)
                .await
                .map_err(|error| error.to_string())?
                == log,
        "repository reopen changed persisted state or log",
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

fn message_entry(sequence: u64, id: &str) -> SessionMutation {
    message_entry_on_lane(sequence, id, None, Some("main"))
}

fn message_entry_on_lane(
    sequence: u64,
    id: &str,
    parent_id: Option<&str>,
    lane: Option<&str>,
) -> SessionMutation {
    let payload = RawValue::from_string("{}".to_owned()).expect("static JSON is valid");
    SessionMutation::Entry {
        lane: lane.map(LaneName::new),
        entry: SessionEntry::Message {
            base: EntryBase {
                id: EntryId::new(id),
                sequence: Sequence::new(sequence),
                parent_id: parent_id.map(EntryId::new),
                timestamp: Timestamp::from_unix_millis(sequence as i64),
            },
            message: AgentRecord::Custom {
                type_name: "conformance".to_owned(),
                payload,
            },
            terminate: false,
        },
    }
}

fn record_base(sequence: u64, id: &str, lane: &str) -> OperationRecordBase {
    OperationRecordBase {
        id: OperationRecordId::new(id),
        sequence: Sequence::new(sequence),
        lane: LaneName::new(lane),
        timestamp: Timestamp::from_unix_millis(sequence as i64),
    }
}

fn operation_started(sequence: u64, id: &str, lane: &str) -> SessionMutation {
    SessionMutation::Record {
        record: OperationRecord::Started {
            base: record_base(sequence, id, lane),
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
                micros: 50,
            }),
            adjustment: None,
        },
    }
}

fn ensure(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

fn block_on_send<F: Future>(future: F) -> F::Output {
    struct ThreadWake(thread::Thread);

    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSessionRepository, InMemorySessionRepository, LocalInMemorySessionRepository};
    use futures_executor::block_on;

    #[test]
    fn memory_send_backend_storage_recovery_conformance() {
        // Pi basis: packages/agent/src/harness/session/testing/conformance.ts.
        let repository = InMemorySessionRepository::new();
        let report = block_on(run_send_storage_conformance(&repository, "memory-send"))
            .expect("in-memory Send backend conformance");
        assert_eq!(report.completed_cases, SESSION_BACKEND_CONFORMANCE_CASES);
    }

    #[test]
    fn memory_local_backend_storage_recovery_conformance() {
        // Pi basis: packages/agent/src/harness/session/testing/conformance.ts.
        let repository = LocalInMemorySessionRepository::new();
        let report = block_on(run_local_storage_conformance(&repository, "memory-local"))
            .expect("in-memory Local backend conformance");
        assert_eq!(report.completed_cases, SESSION_BACKEND_CONFORMANCE_CASES);
    }

    #[test]
    fn file_send_backend_storage_recovery_conformance() {
        // Pi basis: packages/agent/src/harness/session/jsonl/storage.ts and
        // session/testing/conformance.ts, for storage protocol semantics only.
        let directory = tempfile::tempdir().expect("temporary session repository");
        let repository = FileSessionRepository::new(directory.path()).expect("file repository");
        let report = block_on(run_send_storage_conformance(&repository, "file-send"))
            .expect("file Send backend conformance");
        assert_eq!(report.completed_cases, SESSION_BACKEND_CONFORMANCE_CASES);
    }

    #[test]
    fn file_local_backend_storage_recovery_conformance() {
        // Pi basis: packages/agent/src/harness/session/jsonl/storage.ts and
        // session/testing/conformance.ts, for storage protocol semantics only.
        let directory = tempfile::tempdir().expect("temporary session repository");
        let repository = FileSessionRepository::new(directory.path()).expect("file repository");
        let report = block_on(run_local_storage_conformance(&repository, "file-local"))
            .expect("file Local backend conformance");
        assert_eq!(report.completed_cases, SESSION_BACKEND_CONFORMANCE_CASES);
    }
}
