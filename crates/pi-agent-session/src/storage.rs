//! Object-safe storage/repository traits and hermetic in-memory backends.

use crate::{
    APPEND_RECEIPT_SCHEMA_VERSION, AppendReceipt, CreateSessionRequest, ForkRequest,
    LocalBoxFuture, Sequence, SessionError, SessionErrorKind, SessionHeader, SessionId,
    SessionMetadata, SessionMutation, SessionQuery, SessionReducer, SessionState,
    TAIL_REPAIR_REPORT_SCHEMA_VERSION, TailRepairReport,
};
use pi_ai::SendBoxFuture;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
};

/// Send-capable object-safe session storage from Architecture v2 part 2 §7.3.
pub trait SessionStorage: Send + Sync + 'static {
    /// Returns current session metadata.
    fn metadata(&self) -> SendBoxFuture<'_, Result<SessionMetadata, SessionError>>;

    /// Loads the complete state derived from the accepted log.
    fn load_state(&self) -> SendBoxFuture<'_, Result<SessionState, SessionError>>;

    /// Atomically appends a batch if the current sequence equals `expected_sequence`.
    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> SendBoxFuture<'_, Result<AppendReceipt, SessionError>>;

    /// Returns accepted mutations after an exclusive sequence bound.
    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> SendBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>>;

    /// Repairs a backend-specific torn final write without hiding interior corruption.
    fn repair_tail(&self) -> SendBoxFuture<'_, Result<TailRepairReport, SessionError>>;
}

/// Local-executor session storage counterpart from Architecture v2 part 2 §9.2.
pub trait LocalSessionStorage: 'static {
    /// Returns current session metadata.
    fn metadata(&self) -> LocalBoxFuture<'_, Result<SessionMetadata, SessionError>>;

    /// Loads the complete state derived from the accepted log.
    fn load_state(&self) -> LocalBoxFuture<'_, Result<SessionState, SessionError>>;

    /// Atomically appends a batch if the current sequence equals `expected_sequence`.
    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<AppendReceipt, SessionError>>;

    /// Returns accepted mutations after an exclusive sequence bound.
    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>>;

    /// Repairs a backend-specific torn final write without hiding interior corruption.
    fn repair_tail(&self) -> LocalBoxFuture<'_, Result<TailRepairReport, SessionError>>;
}

/// Send-capable object-safe repository from Architecture v2 part 2 §7.3.
pub trait SessionRepository: Send + Sync + 'static {
    /// Creates an empty session.
    fn create(
        &self,
        request: CreateSessionRequest,
    ) -> SendBoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>>;

    /// Opens an existing session.
    fn open(
        &self,
        id: &SessionId,
    ) -> SendBoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>>;

    /// Forks a branch prefix or complete immutable tree into a new session.
    fn fork(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> SendBoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>>;

    /// Lists session metadata matching a repository query.
    fn list(
        &self,
        query: SessionQuery,
    ) -> SendBoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>>;
}

/// Local-executor repository counterpart from Architecture v2 part 2 §9.2.
pub trait LocalSessionRepository: 'static {
    /// Creates an empty session.
    fn create(
        &self,
        request: CreateSessionRequest,
    ) -> LocalBoxFuture<'_, Result<Rc<dyn LocalSessionStorage>, SessionError>>;

    /// Opens an existing session.
    fn open(
        &self,
        id: &SessionId,
    ) -> LocalBoxFuture<'_, Result<Rc<dyn LocalSessionStorage>, SessionError>>;

    /// Forks a branch prefix or complete immutable tree into a new session.
    fn fork(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> LocalBoxFuture<'_, Result<Rc<dyn LocalSessionStorage>, SessionError>>;

    /// Lists session metadata matching a repository query.
    fn list(
        &self,
        query: SessionQuery,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>>;
}

/// Hermetic process-local storage with serialized atomic append batches.
pub struct InMemorySessionStorage {
    header: SessionHeader,
    state: Mutex<SessionState>,
}

impl InMemorySessionStorage {
    /// Creates empty storage for a validated native header.
    pub fn new(header: SessionHeader) -> Result<Self, SessionError> {
        if header.schema_version != crate::SESSION_HEADER_SCHEMA_VERSION {
            return Err(SessionError::new(
                SessionErrorKind::Corruption,
                format!(
                    "unsupported native session header schema {}",
                    header.schema_version
                ),
            ));
        }
        Ok(Self {
            header,
            state: Mutex::new(SessionState::new()),
        })
    }

    /// Returns a detached state snapshot.
    pub fn state_snapshot(&self) -> Result<SessionState, SessionError> {
        Ok(self.lock_state()?.clone())
    }

    /// Atomically applies a complete mutation batch synchronously.
    pub fn append_batch(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> Result<AppendReceipt, SessionError> {
        let mut state = self.lock_state()?;
        if state.sequence() != expected_sequence {
            return Err(SessionError::sequence_conflict(
                expected_sequence,
                state.sequence(),
            ));
        }
        let mut staged = state.clone();
        for mutation in &mutations {
            if let SessionMutation::Record {
                record: crate::OperationRecord::Started { base, .. },
            } = mutation
                && !staged.open_operations(&base.lane).is_empty()
            {
                return Err(SessionError::new(
                    SessionErrorKind::Storage,
                    format!("lane {} already has an open operation", base.lane),
                ));
            }
            staged.apply(mutation)?;
        }
        let receipt = AppendReceipt {
            schema_version: APPEND_RECEIPT_SCHEMA_VERSION,
            previous_sequence: expected_sequence,
            last_sequence: staged.sequence(),
            mutation_count: mutations.len(),
        };
        *state = staged;
        Ok(receipt)
    }

    /// Returns current metadata synchronously.
    pub fn metadata_snapshot(&self) -> Result<SessionMetadata, SessionError> {
        let state = self.lock_state()?;
        Ok(SessionMetadata {
            schema_version: crate::SESSION_METADATA_SCHEMA_VERSION,
            session_id: self.header.session_id.clone(),
            created_at: self.header.created_at,
            parent_session_id: self.header.parent_session_id.clone(),
            environment: self.header.environment.clone(),
            last_sequence: state.sequence(),
        })
    }

    /// Returns a bounded detached log synchronously.
    pub fn log_snapshot(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> Result<Vec<SessionMutation>, SessionError> {
        validate_limit(limit)?;
        let state = self.lock_state()?;
        Ok(state
            .log()
            .iter()
            .filter(|mutation| after.is_none_or(|sequence| mutation.sequence() > sequence))
            .take(limit.unwrap_or(usize::MAX))
            .cloned()
            .collect())
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, SessionState>, SessionError> {
        self.state.lock().map_err(|_| {
            SessionError::new(
                SessionErrorKind::Storage,
                "in-memory session lock was poisoned",
            )
        })
    }
}

impl SessionStorage for InMemorySessionStorage {
    fn metadata(&self) -> SendBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        Box::pin(async move { self.metadata_snapshot() })
    }

    fn load_state(&self) -> SendBoxFuture<'_, Result<SessionState, SessionError>> {
        Box::pin(async move { self.state_snapshot() })
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> SendBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        Box::pin(async move { self.append_batch(expected_sequence, mutations) })
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> SendBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        Box::pin(async move { self.log_snapshot(after, limit) })
    }

    fn repair_tail(&self) -> SendBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        Box::pin(async move {
            Ok(TailRepairReport {
                schema_version: TAIL_REPAIR_REPORT_SCHEMA_VERSION,
                repaired: false,
                removed_bytes: 0,
                last_sequence: self.lock_state()?.sequence(),
            })
        })
    }
}

impl LocalSessionStorage for InMemorySessionStorage {
    fn metadata(&self) -> LocalBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        Box::pin(async move { self.metadata_snapshot() })
    }

    fn load_state(&self) -> LocalBoxFuture<'_, Result<SessionState, SessionError>> {
        Box::pin(async move { self.state_snapshot() })
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        Box::pin(async move { self.append_batch(expected_sequence, mutations) })
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        Box::pin(async move { self.log_snapshot(after, limit) })
    }

    fn repair_tail(&self) -> LocalBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        Box::pin(async move {
            Ok(TailRepairReport {
                schema_version: TAIL_REPAIR_REPORT_SCHEMA_VERSION,
                repaired: false,
                removed_bytes: 0,
                last_sequence: self.lock_state()?.sequence(),
            })
        })
    }
}

/// Send-capable process-local session repository.
#[derive(Default)]
pub struct InMemorySessionRepository {
    sessions: Mutex<BTreeMap<SessionId, Arc<InMemorySessionStorage>>>,
}

impl InMemorySessionRepository {
    /// Creates an empty repository.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_sessions(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<SessionId, Arc<InMemorySessionStorage>>>, SessionError>
    {
        self.sessions.lock().map_err(|_| {
            SessionError::new(
                SessionErrorKind::Storage,
                "in-memory session repository lock was poisoned",
            )
        })
    }

    fn create_sync(
        &self,
        request: CreateSessionRequest,
    ) -> Result<Arc<dyn SessionStorage>, SessionError> {
        let header = request.into_header();
        let id = header.session_id.clone();
        let storage = Arc::new(InMemorySessionStorage::new(header)?);
        let mut sessions = self.lock_sessions()?;
        if sessions.contains_key(&id) {
            return Err(SessionError::new(
                SessionErrorKind::AlreadyExists,
                format!("session already exists: {id}"),
            ));
        }
        sessions.insert(id, storage.clone());
        Ok(storage)
    }

    fn open_concrete(&self, id: &SessionId) -> Result<Arc<InMemorySessionStorage>, SessionError> {
        self.lock_sessions()?.get(id).cloned().ok_or_else(|| {
            SessionError::new(
                SessionErrorKind::NotFound,
                format!("session not found: {id}"),
            )
        })
    }

    fn fork_sync(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> Result<Arc<dyn SessionStorage>, SessionError> {
        let source_storage = self.open_concrete(source)?;
        let source_state = source_storage.state_snapshot()?;
        let mutations = source_state.create_fork_mutations(&request.position)?;
        let header = SessionHeader {
            schema_version: crate::SESSION_HEADER_SCHEMA_VERSION,
            session_id: request.session_id.clone(),
            created_at: request.created_at,
            parent_session_id: Some(source.clone()),
            environment: request.environment,
        };
        let destination = Arc::new(InMemorySessionStorage::new(header)?);
        destination.append_batch(Sequence::ZERO, mutations)?;

        let mut sessions = self.lock_sessions()?;
        if sessions.contains_key(&request.session_id) {
            return Err(SessionError::new(
                SessionErrorKind::AlreadyExists,
                format!("session already exists: {}", request.session_id),
            ));
        }
        sessions.insert(request.session_id, destination.clone());
        Ok(destination)
    }

    fn list_sync(&self, query: SessionQuery) -> Result<Vec<SessionMetadata>, SessionError> {
        validate_limit(query.limit)?;
        let sessions = self.lock_sessions()?;
        let mut metadata = sessions
            .values()
            .map(|storage| storage.metadata_snapshot())
            .collect::<Result<Vec<_>, _>>()?;
        metadata.retain(|item| {
            query
                .parent_session_id
                .as_ref()
                .is_none_or(|parent| item.parent_session_id.as_ref() == Some(parent))
        });
        metadata.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        metadata.truncate(query.limit.unwrap_or(usize::MAX));
        Ok(metadata)
    }
}

impl SessionRepository for InMemorySessionRepository {
    fn create(
        &self,
        request: CreateSessionRequest,
    ) -> SendBoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        Box::pin(async move { self.create_sync(request) })
    }

    fn open(
        &self,
        id: &SessionId,
    ) -> SendBoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let result = self
            .open_concrete(id)
            .map(|storage| storage as Arc<dyn SessionStorage>);
        Box::pin(async move { result })
    }

    fn fork(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> SendBoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let result = self.fork_sync(source, request);
        Box::pin(async move { result })
    }

    fn list(
        &self,
        query: SessionQuery,
    ) -> SendBoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        Box::pin(async move { self.list_sync(query) })
    }
}

/// Local-executor process-local session repository.
#[derive(Default)]
pub struct LocalInMemorySessionRepository {
    sessions: RefCell<BTreeMap<SessionId, Rc<InMemorySessionStorage>>>,
}

impl LocalInMemorySessionRepository {
    /// Creates an empty local repository.
    pub fn new() -> Self {
        Self::default()
    }

    fn create_sync(
        &self,
        request: CreateSessionRequest,
    ) -> Result<Rc<dyn LocalSessionStorage>, SessionError> {
        let header = request.into_header();
        let id = header.session_id.clone();
        let storage = Rc::new(InMemorySessionStorage::new(header)?);
        let mut sessions = self.sessions.borrow_mut();
        if sessions.contains_key(&id) {
            return Err(SessionError::new(
                SessionErrorKind::AlreadyExists,
                format!("session already exists: {id}"),
            ));
        }
        sessions.insert(id, storage.clone());
        Ok(storage)
    }

    fn open_concrete(&self, id: &SessionId) -> Result<Rc<InMemorySessionStorage>, SessionError> {
        self.sessions.borrow().get(id).cloned().ok_or_else(|| {
            SessionError::new(
                SessionErrorKind::NotFound,
                format!("session not found: {id}"),
            )
        })
    }

    fn fork_sync(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> Result<Rc<dyn LocalSessionStorage>, SessionError> {
        let source_storage = self.open_concrete(source)?;
        let mutations = source_storage
            .state_snapshot()?
            .create_fork_mutations(&request.position)?;
        let header = SessionHeader {
            schema_version: crate::SESSION_HEADER_SCHEMA_VERSION,
            session_id: request.session_id.clone(),
            created_at: request.created_at,
            parent_session_id: Some(source.clone()),
            environment: request.environment,
        };
        let destination = Rc::new(InMemorySessionStorage::new(header)?);
        destination.append_batch(Sequence::ZERO, mutations)?;

        let mut sessions = self.sessions.borrow_mut();
        if sessions.contains_key(&request.session_id) {
            return Err(SessionError::new(
                SessionErrorKind::AlreadyExists,
                format!("session already exists: {}", request.session_id),
            ));
        }
        sessions.insert(request.session_id, destination.clone());
        Ok(destination)
    }

    fn list_sync(&self, query: SessionQuery) -> Result<Vec<SessionMetadata>, SessionError> {
        validate_limit(query.limit)?;
        let mut metadata = self
            .sessions
            .borrow()
            .values()
            .map(|storage| storage.metadata_snapshot())
            .collect::<Result<Vec<_>, _>>()?;
        metadata.retain(|item| {
            query
                .parent_session_id
                .as_ref()
                .is_none_or(|parent| item.parent_session_id.as_ref() == Some(parent))
        });
        metadata.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        metadata.truncate(query.limit.unwrap_or(usize::MAX));
        Ok(metadata)
    }
}

impl LocalSessionRepository for LocalInMemorySessionRepository {
    fn create(
        &self,
        request: CreateSessionRequest,
    ) -> LocalBoxFuture<'_, Result<Rc<dyn LocalSessionStorage>, SessionError>> {
        Box::pin(async move { self.create_sync(request) })
    }

    fn open(
        &self,
        id: &SessionId,
    ) -> LocalBoxFuture<'_, Result<Rc<dyn LocalSessionStorage>, SessionError>> {
        let result = self
            .open_concrete(id)
            .map(|storage| storage as Rc<dyn LocalSessionStorage>);
        Box::pin(async move { result })
    }

    fn fork(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> LocalBoxFuture<'_, Result<Rc<dyn LocalSessionStorage>, SessionError>> {
        let result = self.fork_sync(source, request);
        Box::pin(async move { result })
    }

    fn list(
        &self,
        query: SessionQuery,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        Box::pin(async move { self.list_sync(query) })
    }
}

fn validate_limit(limit: Option<usize>) -> Result<(), SessionError> {
    if limit == Some(0) {
        Err(SessionError::new(
            SessionErrorKind::InvalidQuery,
            "limit must be a positive integer",
        ))
    } else {
        Ok(())
    }
}
