//! Native file-backed session storage.

use crate::{
    APPEND_RECEIPT_SCHEMA_VERSION, AppendReceipt, CreateSessionRequest, ForkRequest,
    LocalBoxFuture, LocalSessionRepository, LocalSessionStorage, Sequence, SessionError,
    SessionErrorKind, SessionHeader, SessionId, SessionMetadata, SessionMutation, SessionQuery,
    SessionReducer, SessionRepository, SessionState, SessionStorage,
    TAIL_REPAIR_REPORT_SCHEMA_VERSION, TailRepairReport,
};
use agentprism_ai::SendBoxFuture;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

/// Current native file-envelope schema.
pub const NATIVE_SESSION_FILE_SCHEMA_VERSION: u32 = 1;

/// Current native mutation-batch schema.
pub const NATIVE_MUTATION_BATCH_SCHEMA_VERSION: u32 = 1;

/// Filename suffix used by the native repository.
pub const NATIVE_SESSION_FILE_EXTENSION: &str = "agentprism-session";

const NATIVE_FORMAT_NAME: &str = "agentprism-session";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativeRecord {
    Header {
        schema_version: u32,
        format: String,
        header: SessionHeader,
    },
    MutationBatch {
        schema_version: u32,
        mutations: Vec<SessionMutation>,
    },
}

#[derive(Debug)]
struct DecodedFile {
    header: SessionHeader,
    state: SessionState,
    repaired_bytes: Option<Vec<u8>>,
    removed_bytes: u64,
}

/// A durable native session backed by one append-only file.
///
/// Every operation acquires a sidecar OS file lock. Appends re-read and validate
/// the complete accepted log while holding that lock, so independent repository
/// instances and processes share one serialized writer order.
#[derive(Debug)]
pub struct FileSessionStorage {
    path: PathBuf,
    header: SessionHeader,
}

impl FileSessionStorage {
    /// Creates a new empty native session file without overwriting an existing one.
    pub fn create(path: impl Into<PathBuf>, header: SessionHeader) -> Result<Self, SessionError> {
        validate_header(&header)?;
        let path = path.into();
        ensure_parent(&path)?;
        let _lock = PathLock::acquire(&path)?;
        if path.exists() {
            return Err(SessionError::new(
                SessionErrorKind::AlreadyExists,
                format!("session file already exists: {}", path.display()),
            ));
        }
        let bytes = encode_file(&header, &[])?;
        publish_atomically(&path, &bytes)?;
        Ok(Self { path, header })
    }

    /// Opens a native session, repairing a torn final batch before returning.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        Self::open_with_repair_report(path).map(|(storage, _)| storage)
    }

    /// Opens a native session and returns the repair performed during that open.
    pub fn open_with_repair_report(
        path: impl Into<PathBuf>,
    ) -> Result<(Self, TailRepairReport), SessionError> {
        let path = path.into();
        let _lock = PathLock::acquire(&path)?;
        let decoded = read_and_repair_locked(&path)?;
        let report = repair_report(&decoded);
        Ok((
            Self {
                path,
                header: decoded.header,
            },
            report,
        ))
    }

    /// Returns the native session path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the immutable file header.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Loads and validates the current state synchronously.
    pub fn load_state_sync(&self) -> Result<SessionState, SessionError> {
        let _lock = PathLock::acquire(&self.path)?;
        let decoded = read_and_repair_locked(&self.path)?;
        self.validate_identity(&decoded.header)?;
        Ok(decoded.state)
    }

    /// Returns current metadata synchronously.
    pub fn metadata_sync(&self) -> Result<SessionMetadata, SessionError> {
        let state = self.load_state_sync()?;
        Ok(SessionMetadata {
            schema_version: crate::SESSION_METADATA_SCHEMA_VERSION,
            session_id: self.header.session_id.clone(),
            created_at: self.header.created_at,
            parent_session_id: self.header.parent_session_id.clone(),
            environment: self.header.environment.clone(),
            last_sequence: state.sequence(),
        })
    }

    /// Atomically appends one native mutation batch synchronously.
    pub fn append_batch_sync(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> Result<AppendReceipt, SessionError> {
        let _lock = PathLock::acquire(&self.path)?;
        let decoded = read_and_repair_locked(&self.path)?;
        self.validate_identity(&decoded.header)?;
        if decoded.state.sequence() != expected_sequence {
            return Err(SessionError::sequence_conflict(
                expected_sequence,
                decoded.state.sequence(),
            ));
        }

        let mut staged = decoded.state;
        let mutation_count = mutations.len();
        validate_live_batch(&mut staged, &mutations)?;
        if !mutations.is_empty() {
            let record = NativeRecord::MutationBatch {
                schema_version: NATIVE_MUTATION_BATCH_SCHEMA_VERSION,
                mutations,
            };
            let mut bytes = encode_record(&record)?;
            bytes.push(b'\n');
            append_durably(&self.path, &bytes)?;
        }
        Ok(AppendReceipt {
            schema_version: APPEND_RECEIPT_SCHEMA_VERSION,
            previous_sequence: expected_sequence,
            last_sequence: staged.sequence(),
            mutation_count,
        })
    }

    /// Returns a bounded detached native log synchronously.
    pub fn log_sync(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> Result<Vec<SessionMutation>, SessionError> {
        validate_limit(limit)?;
        let state = self.load_state_sync()?;
        Ok(state
            .log()
            .iter()
            .filter(|mutation| after.is_none_or(|sequence| mutation.sequence() > sequence))
            .take(limit.unwrap_or(usize::MAX))
            .cloned()
            .collect())
    }

    /// Detects and repairs a torn final native batch synchronously.
    pub fn repair_tail_sync(&self) -> Result<TailRepairReport, SessionError> {
        let _lock = PathLock::acquire(&self.path)?;
        let decoded = read_and_repair_locked(&self.path)?;
        self.validate_identity(&decoded.header)?;
        Ok(repair_report(&decoded))
    }

    fn create_with_mutations(
        path: PathBuf,
        header: SessionHeader,
        mutations: Vec<SessionMutation>,
    ) -> Result<Self, SessionError> {
        validate_header(&header)?;
        let mut state = SessionState::new();
        validate_live_batch(&mut state, &mutations)?;
        ensure_parent(&path)?;
        let _lock = PathLock::acquire(&path)?;
        if path.exists() {
            return Err(SessionError::new(
                SessionErrorKind::AlreadyExists,
                format!("session file already exists: {}", path.display()),
            ));
        }
        let batches = if mutations.is_empty() {
            Vec::new()
        } else {
            vec![mutations]
        };
        let bytes = encode_file(&header, &batches)?;
        publish_atomically(&path, &bytes)?;
        Ok(Self { path, header })
    }

    fn validate_identity(&self, header: &SessionHeader) -> Result<(), SessionError> {
        if header == &self.header {
            Ok(())
        } else {
            Err(SessionError::new(
                SessionErrorKind::Corruption,
                format!(
                    "session header changed while storage was open: {}",
                    self.path.display()
                ),
            ))
        }
    }
}

impl SessionStorage for FileSessionStorage {
    fn metadata(&self) -> SendBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        Box::pin(async move { self.metadata_sync() })
    }

    fn load_state(&self) -> SendBoxFuture<'_, Result<SessionState, SessionError>> {
        Box::pin(async move { self.load_state_sync() })
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> SendBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        Box::pin(async move { self.append_batch_sync(expected_sequence, mutations) })
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> SendBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        Box::pin(async move { self.log_sync(after, limit) })
    }

    fn repair_tail(&self) -> SendBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        Box::pin(async move { self.repair_tail_sync() })
    }
}

impl LocalSessionStorage for FileSessionStorage {
    fn metadata(&self) -> LocalBoxFuture<'_, Result<SessionMetadata, SessionError>> {
        Box::pin(async move { self.metadata_sync() })
    }

    fn load_state(&self) -> LocalBoxFuture<'_, Result<SessionState, SessionError>> {
        Box::pin(async move { self.load_state_sync() })
    }

    fn append(
        &self,
        expected_sequence: Sequence,
        mutations: Vec<SessionMutation>,
    ) -> LocalBoxFuture<'_, Result<AppendReceipt, SessionError>> {
        Box::pin(async move { self.append_batch_sync(expected_sequence, mutations) })
    }

    fn log(
        &self,
        after: Option<Sequence>,
        limit: Option<usize>,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMutation>, SessionError>> {
        Box::pin(async move { self.log_sync(after, limit) })
    }

    fn repair_tail(&self) -> LocalBoxFuture<'_, Result<TailRepairReport, SessionError>> {
        Box::pin(async move { self.repair_tail_sync() })
    }
}

/// Native file-backed session repository rooted at one directory.
#[derive(Clone, Debug)]
pub struct FileSessionRepository {
    root: Arc<PathBuf>,
}

impl FileSessionRepository {
    /// Opens or creates a repository root directory.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|error| io_error("create session repository", error))?;
        Ok(Self {
            root: Arc::new(root),
        })
    }

    /// Returns the repository root.
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Returns the native path for a validated session identifier.
    pub fn session_path(&self, id: &SessionId) -> Result<PathBuf, SessionError> {
        validate_session_id(id)?;
        Ok(self
            .root
            .join(format!("{}.{}", id.as_str(), NATIVE_SESSION_FILE_EXTENSION)))
    }

    /// Deletes one session idempotently.
    pub fn delete(&self, id: &SessionId) -> Result<(), SessionError> {
        let path = self.session_path(id)?;
        let _lock = PathLock::acquire(&path)?;
        match fs::remove_file(&path) {
            Ok(()) => sync_parent(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("delete session", error)),
        }
    }

    fn create_sync(
        &self,
        request: CreateSessionRequest,
    ) -> Result<Arc<dyn SessionStorage>, SessionError> {
        let path = self.session_path(&request.session_id)?;
        Ok(Arc::new(FileSessionStorage::create(
            path,
            request.into_header(),
        )?))
    }

    fn open_concrete(&self, id: &SessionId) -> Result<FileSessionStorage, SessionError> {
        let path = self.session_path(id)?;
        if !path.exists() {
            return Err(SessionError::new(
                SessionErrorKind::NotFound,
                format!("session not found: {id}"),
            ));
        }
        let storage = FileSessionStorage::open(path)?;
        if storage.header.session_id != *id {
            return Err(SessionError::new(
                SessionErrorKind::Corruption,
                format!("session id does not match native header: {id}"),
            ));
        }
        Ok(storage)
    }

    fn fork_concrete(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> Result<FileSessionStorage, SessionError> {
        let source_storage = self.open_concrete(source)?;
        let mutations = source_storage
            .load_state_sync()?
            .create_fork_mutations(&request.position)?;
        let path = self.session_path(&request.session_id)?;
        let header = SessionHeader {
            schema_version: crate::SESSION_HEADER_SCHEMA_VERSION,
            session_id: request.session_id,
            created_at: request.created_at,
            parent_session_id: Some(source.clone()),
            environment: request.environment,
        };
        FileSessionStorage::create_with_mutations(path, header, mutations)
    }

    fn list_sync(&self, query: SessionQuery) -> Result<Vec<SessionMetadata>, SessionError> {
        validate_limit(query.limit)?;
        let mut metadata = Vec::new();
        for entry in
            fs::read_dir(self.root.as_path()).map_err(|error| io_error("list sessions", error))?
        {
            let entry = entry.map_err(|error| io_error("read session directory entry", error))?;
            let path = entry.path();
            if !entry
                .file_type()
                .map_err(|error| io_error("read session file type", error))?
                .is_file()
                || path.extension().and_then(|value| value.to_str())
                    != Some(NATIVE_SESSION_FILE_EXTENSION)
            {
                continue;
            }
            let storage = FileSessionStorage::open(path)?;
            let item = storage.metadata_sync()?;
            if query
                .parent_session_id
                .as_ref()
                .is_none_or(|parent| item.parent_session_id.as_ref() == Some(parent))
            {
                metadata.push(item);
            }
        }
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

impl SessionRepository for FileSessionRepository {
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
            .map(|storage| Arc::new(storage) as Arc<dyn SessionStorage>);
        Box::pin(async move { result })
    }

    fn fork(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> SendBoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let result = self
            .fork_concrete(source, request)
            .map(|storage| Arc::new(storage) as Arc<dyn SessionStorage>);
        Box::pin(async move { result })
    }

    fn list(
        &self,
        query: SessionQuery,
    ) -> SendBoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        Box::pin(async move { self.list_sync(query) })
    }
}

impl LocalSessionRepository for FileSessionRepository {
    fn create(
        &self,
        request: CreateSessionRequest,
    ) -> LocalBoxFuture<'_, Result<Rc<dyn LocalSessionStorage>, SessionError>> {
        let result = (|| {
            let path = self.session_path(&request.session_id)?;
            Ok(
                Rc::new(FileSessionStorage::create(path, request.into_header())?)
                    as Rc<dyn LocalSessionStorage>,
            )
        })();
        Box::pin(async move { result })
    }

    fn open(
        &self,
        id: &SessionId,
    ) -> LocalBoxFuture<'_, Result<Rc<dyn LocalSessionStorage>, SessionError>> {
        let result = self
            .open_concrete(id)
            .map(|storage| Rc::new(storage) as Rc<dyn LocalSessionStorage>);
        Box::pin(async move { result })
    }

    fn fork(
        &self,
        source: &SessionId,
        request: ForkRequest,
    ) -> LocalBoxFuture<'_, Result<Rc<dyn LocalSessionStorage>, SessionError>> {
        let result = self
            .fork_concrete(source, request)
            .map(|storage| Rc::new(storage) as Rc<dyn LocalSessionStorage>);
        Box::pin(async move { result })
    }

    fn list(
        &self,
        query: SessionQuery,
    ) -> LocalBoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        Box::pin(async move { self.list_sync(query) })
    }
}

fn validate_live_batch(
    state: &mut SessionState,
    mutations: &[SessionMutation],
) -> Result<(), SessionError> {
    for mutation in mutations {
        if let SessionMutation::Record {
            record: crate::OperationRecord::Started { base, .. },
        } = mutation
            && !state.open_operations(&base.lane).is_empty()
        {
            return Err(SessionError::new(
                SessionErrorKind::Storage,
                format!("lane {} already has an open operation", base.lane),
            ));
        }
        state.apply(mutation)?;
    }
    Ok(())
}

fn validate_header(header: &SessionHeader) -> Result<(), SessionError> {
    if header.schema_version != crate::SESSION_HEADER_SCHEMA_VERSION {
        return Err(SessionError::new(
            SessionErrorKind::Corruption,
            format!(
                "unsupported native session header schema {}",
                header.schema_version
            ),
        ));
    }
    validate_session_id(&header.session_id)
}

fn validate_session_id(id: &SessionId) -> Result<(), SessionError> {
    let value = id.as_str();
    let valid_edge = |byte: u8| byte.is_ascii_alphanumeric();
    let valid = value.as_bytes().first().copied().is_some_and(valid_edge)
        && value.as_bytes().last().copied().is_some_and(valid_edge)
        && value
            .bytes()
            .all(|byte| valid_edge(byte) || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(SessionError::new(
            SessionErrorKind::InvalidPayload,
            "session id must start and end with an ASCII alphanumeric character and contain only '.', '_', or '-' separators",
        ))
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

fn encode_file(
    header: &SessionHeader,
    batches: &[Vec<SessionMutation>],
) -> Result<Vec<u8>, SessionError> {
    let mut bytes = encode_record(&NativeRecord::Header {
        schema_version: NATIVE_SESSION_FILE_SCHEMA_VERSION,
        format: NATIVE_FORMAT_NAME.to_owned(),
        header: header.clone(),
    })?;
    bytes.push(b'\n');
    for mutations in batches {
        bytes.extend(encode_record(&NativeRecord::MutationBatch {
            schema_version: NATIVE_MUTATION_BATCH_SCHEMA_VERSION,
            mutations: mutations.clone(),
        })?);
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn encode_record(record: &NativeRecord) -> Result<Vec<u8>, SessionError> {
    serde_json::to_vec(record).map_err(|error| {
        SessionError::new(
            SessionErrorKind::Storage,
            format!("failed to encode native session record: {error}"),
        )
    })
}

fn read_and_repair_locked(path: &Path) -> Result<DecodedFile, SessionError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SessionError::new(
                    SessionErrorKind::NotFound,
                    format!("session file not found: {}", path.display()),
                )
            } else {
                io_error("open session", error)
            }
        })?
        .read_to_end(&mut bytes)
        .map_err(|error| io_error("read session", error))?;
    let decoded = decode_file(&bytes)?;
    if let Some(repaired) = &decoded.repaired_bytes {
        publish_atomically(path, repaired)?;
    }
    Ok(decoded)
}

fn decode_file(bytes: &[u8]) -> Result<DecodedFile, SessionError> {
    if bytes.is_empty() {
        return Err(SessionError::new(
            SessionErrorKind::Corruption,
            "native session file is empty",
        ));
    }
    let ranges = line_ranges(bytes);
    let mut header = None;
    let mut state = SessionState::new();
    let mut repaired_bytes = None;
    let mut removed_bytes = 0;

    for (index, (start, end, terminated)) in ranges.iter().copied().enumerate() {
        let content_end = if terminated { end - 1 } else { end };
        let line = &bytes[start..content_end];
        let is_last = index + 1 == ranges.len();
        let record = match serde_json::from_slice::<NativeRecord>(line) {
            Ok(record) => record,
            Err(error)
                if index > 0
                    && is_last
                    && matches!(
                        error.classify(),
                        serde_json::error::Category::Syntax | serde_json::error::Category::Eof
                    ) =>
            {
                let mut prefix = bytes[..start].to_vec();
                if !prefix.ends_with(b"\n") {
                    prefix.push(b'\n');
                }
                removed_bytes = (bytes.len() - start) as u64;
                repaired_bytes = Some(prefix);
                break;
            }
            Err(error) => {
                return Err(SessionError::new(
                    SessionErrorKind::Corruption,
                    format!(
                        "invalid native session record on line {}: {error}",
                        index + 1
                    ),
                ));
            }
        };

        match record {
            NativeRecord::Header {
                schema_version,
                format,
                header: decoded_header,
            } if index == 0 => {
                if schema_version != NATIVE_SESSION_FILE_SCHEMA_VERSION
                    || format != NATIVE_FORMAT_NAME
                {
                    return Err(SessionError::new(
                        SessionErrorKind::Corruption,
                        "unsupported native session file format",
                    ));
                }
                validate_header(&decoded_header)?;
                header = Some(decoded_header);
            }
            NativeRecord::Header { .. } => {
                return Err(SessionError::new(
                    SessionErrorKind::Corruption,
                    format!(
                        "native session has a non-leading header on line {}",
                        index + 1
                    ),
                ));
            }
            NativeRecord::MutationBatch {
                schema_version,
                mutations,
            } => {
                if index == 0 {
                    return Err(SessionError::new(
                        SessionErrorKind::Corruption,
                        "native session is missing its leading header",
                    ));
                }
                if schema_version != NATIVE_MUTATION_BATCH_SCHEMA_VERSION {
                    return Err(SessionError::new(
                        SessionErrorKind::Corruption,
                        format!("unsupported mutation batch schema {schema_version}"),
                    ));
                }
                for mutation in mutations {
                    state.apply(&mutation).map_err(|error| {
                        SessionError::new(
                            SessionErrorKind::Corruption,
                            format!(
                                "invalid native session mutation on line {}: {error}",
                                index + 1
                            ),
                        )
                    })?;
                }
            }
        }

        if is_last && !terminated {
            let mut repaired = bytes.to_vec();
            repaired.push(b'\n');
            repaired_bytes = Some(repaired);
        }
    }

    let header = header.ok_or_else(|| {
        SessionError::new(
            SessionErrorKind::Corruption,
            "native session is missing its leading header",
        )
    })?;
    Ok(DecodedFile {
        header,
        state,
        repaired_bytes,
        removed_bytes,
    })
}

fn line_ranges(bytes: &[u8]) -> Vec<(usize, usize, bool)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            ranges.push((start, index + 1, true));
            start = index + 1;
        }
    }
    if start < bytes.len() {
        ranges.push((start, bytes.len(), false));
    }
    ranges
}

fn repair_report(decoded: &DecodedFile) -> TailRepairReport {
    TailRepairReport {
        schema_version: TAIL_REPAIR_REPORT_SCHEMA_VERSION,
        repaired: decoded.repaired_bytes.is_some(),
        removed_bytes: decoded.removed_bytes,
        last_sequence: decoded.state.sequence(),
    }
}

fn append_durably(path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .map_err(|error| io_error("open session for append", error))?;
    file.write_all(bytes)
        .map_err(|error| io_error("append session", error))?;
    file.sync_data()
        .map_err(|error| io_error("sync appended session", error))
}

fn publish_atomically(path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
    let temp = sidecar_path(path, ".tmp");
    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)
            .map_err(|error| io_error("create staged session", error))?;
        file.write_all(bytes)
            .map_err(|error| io_error("write staged session", error))?;
        file.sync_all()
            .map_err(|error| io_error("sync staged session", error))?;
        replace_file(&temp, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(temp: &Path, destination: &Path) -> Result<(), SessionError> {
    fs::rename(temp, destination).map_err(|error| io_error("publish staged session", error))
}

#[cfg(windows)]
fn replace_file(temp: &Path, destination: &Path) -> Result<(), SessionError> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    if !destination.exists() {
        return fs::rename(temp, destination)
            .map_err(|error| io_error("publish staged session", error));
    }
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let temp = temp
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both paths are NUL-terminated for the duration of the call; the
    // optional backup and metadata pointers are intentionally null.
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            temp.as_ptr(),
            ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            ptr::null(),
            ptr::null(),
        )
    };
    if replaced == 0 {
        Err(io_error(
            "publish staged session",
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
}

fn sync_parent(path: &Path) -> Result<(), SessionError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    match File::open(parent) {
        Ok(directory) => directory
            .sync_all()
            .map_err(|error| io_error("sync session directory", error)),
        Err(_) if cfg!(windows) => Ok(()),
        Err(error) => Err(io_error("open session directory for sync", error)),
    }
}

fn ensure_parent(path: &Path) -> Result<(), SessionError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error("create session directory", error))?;
    }
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

struct PathLock(File);

impl PathLock {
    fn acquire(path: &Path) -> Result<Self, SessionError> {
        let lock_path = sidecar_path(path, ".lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|error| io_error("open session append lock", error))?;
        file.lock()
            .map_err(|error| io_error("acquire session append lock", error))?;
        Ok(Self(file))
    }
}

impl Drop for PathLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn io_error(context: &str, error: std::io::Error) -> SessionError {
    SessionError::new(SessionErrorKind::Storage, format!("{context}: {error}"))
}
