//! Portable environment capabilities for the agent harness.
//!
//! This crate contains only executor-neutral contracts and data. Native Tokio
//! implementations live in `pi-agent-runtime-tokio`, as required by
//! Architecture v2 part 2 §7.10 and §9.2–§9.6.

#![deny(missing_docs)]

use agentprism_ai::{
    CancellationToken, LocalBoxFuture, LocalBoxStream, SendBoxFuture, SendBoxStream, Timestamp,
};
use bytes::Bytes;
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

/// A path in an [`AgentFileSystem`] namespace.
///
/// Relative values are interpreted against the implementation's configured
/// working directory. The value is addressed rather than canonical: it does
/// not imply that symlinks were followed.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AgentPath(PathBuf);

impl AgentPath {
    /// Creates an addressed path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Borrows the native path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consumes the wrapper and returns its native path.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl<T> From<T> for AgentPath
where
    T: Into<PathBuf>,
{
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl AsRef<Path> for AgentPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl fmt::Display for AgentPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}

/// An existing path after filesystem canonicalization and symlink resolution.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalPath(PathBuf);

impl CanonicalPath {
    /// Creates a canonical-path value from an implementation-verified path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Borrows the native path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consumes the wrapper and returns its native path.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for CanonicalPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl fmt::Display for CanonicalPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}

/// Read bounds applied before a file is materialized in memory.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReadLimits {
    /// Maximum number of bytes returned. `None` reads the complete file.
    pub max_bytes: Option<u64>,
}

/// Result of one bounded filesystem read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileReadResult {
    /// Addressed path that was read.
    pub path: AgentPath,
    /// Bytes retained under [`ReadLimits`].
    pub data: Bytes,
    /// Best available full file length at read time.
    pub total_bytes: u64,
    /// Whether bytes were omitted by the configured limit.
    pub truncated: bool,
}

/// Result of one filesystem write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileWriteResult {
    /// Addressed path that was written.
    pub path: AgentPath,
    /// Number of bytes accepted by the filesystem.
    pub bytes_written: u64,
}

/// Result of an exact text replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditResult {
    /// Addressed path atomically replaced.
    pub path: AgentPath,
    /// Number of exact occurrences replaced. Successful native operations use
    /// one; the field leaves room for explicitly selected future policies.
    pub replacements: u32,
    /// Size of the resulting file.
    pub bytes_written: u64,
}

/// A capability absent from the selected host environment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityUnavailable {
    /// Stable capability identifier such as `process_spawn`.
    pub capability: &'static str,
    /// Sanitized host-facing explanation.
    pub message: String,
}

impl CapabilityUnavailable {
    /// Creates an unavailable-capability report.
    pub fn new(capability: &'static str, message: impl Into<String>) -> Self {
        Self {
            capability,
            message: message.into(),
        }
    }
}

impl fmt::Display for CapabilityUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "capability {} is unavailable: {}",
            self.capability, self.message
        )
    }
}

impl std::error::Error for CapabilityUnavailable {}

/// Portable filesystem failure.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileSystemError {
    /// The operation was cancelled.
    Cancelled {
        /// Addressed path of the cancelled operation.
        path: AgentPath,
    },
    /// The addressed object does not exist.
    NotFound {
        /// Addressed path that was not found.
        path: AgentPath,
        /// Sanitized operating-system diagnostic.
        message: String,
    },
    /// Host permissions rejected the operation.
    PermissionDenied {
        /// Addressed path rejected by host permissions.
        path: AgentPath,
        /// Sanitized operating-system diagnostic.
        message: String,
    },
    /// A directory was required.
    NotDirectory {
        /// Addressed path that was expected to be a directory.
        path: AgentPath,
        /// Sanitized operating-system diagnostic.
        message: String,
    },
    /// A non-directory object was required.
    IsDirectory {
        /// Addressed path that unexpectedly named a directory.
        path: AgentPath,
        /// Sanitized operating-system diagnostic.
        message: String,
    },
    /// The path or file data is invalid for the requested operation.
    Invalid {
        /// Addressed path associated with invalid input.
        path: AgentPath,
        /// Stable validation diagnostic.
        message: String,
    },
    /// Exact replacement found no occurrence.
    ExactMatchNotFound {
        /// Addressed file searched for the exact text.
        path: AgentPath,
    },
    /// Exact replacement found more than one occurrence.
    MultipleExactMatches {
        /// Addressed file containing ambiguous occurrences.
        path: AgentPath,
        /// Number of non-overlapping exact occurrences.
        matches: u64,
    },
    /// Exact replacement would not change the file.
    NoOpReplacement {
        /// Addressed file for which replacement would be a no-op.
        path: AgentPath,
    },
    /// The host does not implement the filesystem capability.
    CapabilityUnavailable(CapabilityUnavailable),
    /// An otherwise unclassified filesystem failure.
    Other {
        /// Addressed path when the host supplied one.
        path: Option<AgentPath>,
        /// Sanitized operating-system diagnostic.
        message: String,
    },
}

impl FileSystemError {
    /// Returns the addressed path associated with this failure, if present.
    pub fn path(&self) -> Option<&AgentPath> {
        match self {
            Self::Cancelled { path }
            | Self::NotFound { path, .. }
            | Self::PermissionDenied { path, .. }
            | Self::NotDirectory { path, .. }
            | Self::IsDirectory { path, .. }
            | Self::Invalid { path, .. }
            | Self::ExactMatchNotFound { path }
            | Self::MultipleExactMatches { path, .. }
            | Self::NoOpReplacement { path } => Some(path),
            Self::CapabilityUnavailable(_) => None,
            Self::Other { path, .. } => path.as_ref(),
        }
    }
}

impl fmt::Display for FileSystemError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { path } => write!(formatter, "filesystem operation cancelled: {path}"),
            Self::NotFound { path, message }
            | Self::PermissionDenied { path, message }
            | Self::NotDirectory { path, message }
            | Self::IsDirectory { path, message }
            | Self::Invalid { path, message } => write!(formatter, "{path}: {message}"),
            Self::ExactMatchNotFound { path } => {
                write!(formatter, "exact replacement text was not found in {path}")
            }
            Self::MultipleExactMatches { path, matches } => {
                write!(
                    formatter,
                    "exact replacement text occurs {matches} times in {path}"
                )
            }
            Self::NoOpReplacement { path } => {
                write!(formatter, "exact replacement would not change {path}")
            }
            Self::CapabilityUnavailable(error) => error.fmt(formatter),
            Self::Other {
                path: Some(path),
                message,
            } => write!(formatter, "{path}: {message}"),
            Self::Other {
                path: None,
                message,
            } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for FileSystemError {}

/// Portable process launch description.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessCommand {
    /// Executable name or path. No shell is inserted implicitly.
    pub program: String,
    /// Arguments passed verbatim to the executable.
    pub arguments: Vec<String>,
    /// Optional working directory.
    pub current_dir: Option<AgentPath>,
    /// Environment entries overlaid after inherited entries.
    pub environment: BTreeMap<String, String>,
    /// Whether to inherit the host process environment.
    pub inherit_environment: bool,
    /// Optional bytes written to stdin before it is closed.
    pub stdin: Option<Bytes>,
    /// Policy used when the spawn cancellation token fires.
    pub cancellation_policy: TerminationPolicy,
}

impl ProcessCommand {
    /// Creates a command with inherited environment and default termination.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            current_dir: None,
            environment: BTreeMap::new(),
            inherit_environment: true,
            stdin: None,
            cancellation_policy: TerminationPolicy::default(),
        }
    }

    /// Replaces the argument list.
    pub fn with_arguments(mut self, arguments: impl IntoIterator<Item = String>) -> Self {
        self.arguments = arguments.into_iter().collect();
        self
    }

    /// Selects the working directory.
    pub fn with_current_dir(mut self, current_dir: impl Into<AgentPath>) -> Self {
        self.current_dir = Some(current_dir.into());
        self
    }
}

/// A portable signal requested by [`TerminationPolicy`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminationSignal {
    /// Conventional graceful termination request (`SIGTERM` on Unix).
    Terminate,
    /// Interactive interruption request (`SIGINT` on Unix).
    Interrupt,
    /// Immediate forced termination (`SIGKILL` on Unix).
    Kill,
}

/// Process-tree termination and trailing-stdio policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminationPolicy {
    /// First signal sent when termination is requested.
    pub graceful_signal: TerminationSignal,
    /// Maximum time allowed after the graceful signal.
    pub graceful_timeout: Duration,
    /// Maximum time allowed after forced termination.
    pub forced_timeout: Duration,
    /// Maximum trailing-stdio window after process exit.
    pub stdio_grace_period: Duration,
    /// Whether signals target the complete process tree.
    pub terminate_process_tree: bool,
}

impl Default for TerminationPolicy {
    fn default() -> Self {
        Self {
            graceful_signal: TerminationSignal::Terminate,
            graceful_timeout: Duration::from_secs(2),
            forced_timeout: Duration::from_secs(2),
            stdio_grace_period: Duration::from_millis(100),
            terminate_process_tree: true,
        }
    }
}

/// Portable child exit status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessExitStatus {
    /// Numeric exit code, absent when the process ended by signal.
    pub code: Option<i32>,
    /// Portable signal label when the platform reports one.
    pub signal: Option<String>,
    /// Whether the status represents successful completion.
    pub success: bool,
}

/// Why a running process reached its final outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessTermination {
    /// The process exited without an environment termination request.
    Exited,
    /// The configured graceful signal caused exit.
    Graceful,
    /// Forced termination was required.
    Forced,
    /// Spawn cancellation initiated termination.
    Cancelled,
}

/// Final process result returned by the event stream and `terminate`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutcome {
    /// Exit status reported by the operating system.
    pub status: ProcessExitStatus,
    /// Termination path used to reach the status.
    pub termination: ProcessTermination,
}

/// One ordered process observation.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessEvent {
    /// Bytes read from standard output.
    Stdout(Bytes),
    /// Bytes read from standard error.
    Stderr(Bytes),
    /// The sole terminal event, emitted after stdio closes or its grace period expires.
    Exited(ProcessOutcome),
}

/// Portable process failure.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessError {
    /// Cancellation was observed before a process could start.
    Cancelled,
    /// The operating system rejected process creation.
    Spawn {
        /// Sanitized operating-system diagnostic.
        message: String,
    },
    /// Process I/O or status observation failed.
    Io {
        /// Sanitized process I/O diagnostic.
        message: String,
    },
    /// The process did not settle before a termination deadline.
    TerminationTimeout {
        /// Whether the forced phase, rather than the graceful phase, timed out.
        forced: bool,
    },
    /// The selected host has no process capability.
    CapabilityUnavailable(CapabilityUnavailable),
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("process operation cancelled"),
            Self::Spawn { message } => write!(formatter, "could not spawn process: {message}"),
            Self::Io { message } => write!(formatter, "process I/O failed: {message}"),
            Self::TerminationTimeout { forced: false } => {
                formatter.write_str("process did not exit during graceful termination")
            }
            Self::TerminationTimeout { forced: true } => {
                formatter.write_str("process did not exit after forced termination")
            }
            Self::CapabilityUnavailable(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ProcessError {}

/// Send process-event stream used by [`RunningProcess`].
pub type ProcessEventStream<'a> = SendBoxStream<'a, Result<ProcessEvent, ProcessError>>;

/// Local process-event stream used by [`LocalRunningProcess`].
pub type LocalProcessEventStream<'a> = LocalBoxStream<'a, Result<ProcessEvent, ProcessError>>;

/// Executor-neutral filesystem capability for native Send runtimes.
pub trait AgentFileSystem: Send + Sync + 'static {
    /// Resolves an existing path and follows symlinks.
    fn canonicalize(
        &self,
        path: &AgentPath,
    ) -> SendBoxFuture<'_, Result<CanonicalPath, FileSystemError>>;

    /// Reads bytes subject to explicit memory limits.
    fn read(
        &self,
        path: &AgentPath,
        limits: ReadLimits,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileReadResult, FileSystemError>>;

    /// Creates or overwrites a file, creating parent directories as needed.
    fn write(
        &self,
        path: &AgentPath,
        data: Bytes,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileWriteResult, FileSystemError>>;

    /// Replaces one exact UTF-8 occurrence and atomically publishes the result.
    fn replace_exact(
        &self,
        path: &AgentPath,
        expected: &str,
        replacement: &str,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<EditResult, FileSystemError>>;
}

/// Single-threaded counterpart of [`AgentFileSystem`].
pub trait LocalAgentFileSystem: 'static {
    /// Resolves an existing path and follows symlinks.
    fn canonicalize(
        &self,
        path: &AgentPath,
    ) -> LocalBoxFuture<'_, Result<CanonicalPath, FileSystemError>>;

    /// Reads bytes subject to explicit memory limits.
    fn read(
        &self,
        path: &AgentPath,
        limits: ReadLimits,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<FileReadResult, FileSystemError>>;

    /// Creates or overwrites a file, creating parent directories as needed.
    fn write(
        &self,
        path: &AgentPath,
        data: Bytes,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<FileWriteResult, FileSystemError>>;

    /// Replaces one exact UTF-8 occurrence and atomically publishes the result.
    fn replace_exact(
        &self,
        path: &AgentPath,
        expected: &str,
        replacement: &str,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<EditResult, FileSystemError>>;
}

/// Native Send process creation capability.
pub trait ProcessSpawner: Send + Sync + 'static {
    /// Starts one process with piped standard streams.
    fn spawn(
        &self,
        command: ProcessCommand,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RunningProcess>, ProcessError>>;
}

/// A native Send process whose work is joined by its owner.
pub trait RunningProcess: Send {
    /// Borrows the ordered output/status stream.
    fn events(&mut self) -> ProcessEventStream<'_>;

    /// Terminates and joins the process according to `policy`.
    fn terminate(
        &mut self,
        policy: TerminationPolicy,
    ) -> SendBoxFuture<'_, Result<ProcessOutcome, ProcessError>>;
}

/// Single-threaded counterpart of [`ProcessSpawner`].
pub trait LocalProcessSpawner: 'static {
    /// Starts one process with piped standard streams.
    fn spawn(
        &self,
        command: ProcessCommand,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalRunningProcess>, ProcessError>>;
}

/// Single-threaded counterpart of [`RunningProcess`].
pub trait LocalRunningProcess {
    /// Borrows the ordered output/status stream.
    fn events(&mut self) -> LocalProcessEventStream<'_>;

    /// Terminates and joins the process according to `policy`.
    fn terminate(
        &mut self,
        policy: TerminationPolicy,
    ) -> LocalBoxFuture<'_, Result<ProcessOutcome, ProcessError>>;
}

/// Executor-neutral clock failure.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockError {
    /// The sleep was cancelled.
    Cancelled,
    /// The selected host has no timer capability.
    CapabilityUnavailable,
}

impl fmt::Display for ClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("clock sleep cancelled"),
            Self::CapabilityUnavailable => formatter.write_str("clock capability is unavailable"),
        }
    }
}

impl std::error::Error for ClockError {}

/// Clock and cancellable timer capability for native Send runtimes.
pub trait Clock: Send + Sync + 'static {
    /// Returns the current wall-clock timestamp.
    fn now(&self) -> Timestamp;

    /// Sleeps without coupling callers to a particular executor.
    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), ClockError>>;
}

/// Single-threaded counterpart of [`Clock`].
pub trait LocalClock: 'static {
    /// Returns the current wall-clock timestamp.
    fn now(&self) -> Timestamp;

    /// Sleeps without coupling callers to a particular executor.
    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), ClockError>>;
}

/// Request for one host-owned temporary artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemporaryArtifactRequest {
    /// Filename prefix used only for diagnostics and discovery.
    pub prefix: String,
    /// Filename suffix such as `.log`.
    pub suffix: String,
}

impl Default for TemporaryArtifactRequest {
    fn default() -> Self {
        Self {
            prefix: "artifact-".into(),
            suffix: String::new(),
        }
    }
}

/// Stable reference to a host-owned temporary artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactRef {
    /// Canonical artifact path.
    pub path: CanonicalPath,
}

/// Temporary-artifact operation failure.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactError {
    /// Artifact creation or removal failed through the filesystem.
    FileSystem(FileSystemError),
    /// The selected host has no temporary-artifact capability.
    CapabilityUnavailable(CapabilityUnavailable),
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileSystem(error) => error.fmt(formatter),
            Self::CapabilityUnavailable(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ArtifactError {}

/// Temporary artifact capability for native Send runtimes.
pub trait TemporaryArtifactStore: Send + Sync + 'static {
    /// Creates an artifact containing `data`.
    fn create(
        &self,
        request: TemporaryArtifactRequest,
        data: Bytes,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ArtifactRef, ArtifactError>>;

    /// Removes a previously created artifact.
    fn remove(
        &self,
        artifact: &ArtifactRef,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), ArtifactError>>;
}

/// Single-threaded counterpart of [`TemporaryArtifactStore`].
pub trait LocalTemporaryArtifactStore: 'static {
    /// Creates an artifact containing `data`.
    fn create(
        &self,
        request: TemporaryArtifactRequest,
        data: Bytes,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<ArtifactRef, ArtifactError>>;

    /// Removes a previously created artifact.
    fn remove(
        &self,
        artifact: &ArtifactRef,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), ArtifactError>>;
}

/// Complete Send environment capability set required by the harness.
pub trait AgentEnvironment: Send + Sync + 'static {
    /// Returns the filesystem capability.
    fn filesystem(&self) -> &dyn AgentFileSystem;
    /// Returns the process capability.
    fn processes(&self) -> &dyn ProcessSpawner;
    /// Returns the clock capability.
    fn clock(&self) -> &dyn Clock;
    /// Returns the temporary-artifact capability.
    fn temporary_artifacts(&self) -> &dyn TemporaryArtifactStore;
}

/// Complete local environment capability set for WASM and local executors.
pub trait LocalAgentEnvironment: 'static {
    /// Returns the filesystem capability.
    fn filesystem(&self) -> &dyn LocalAgentFileSystem;
    /// Returns the process capability.
    fn processes(&self) -> &dyn LocalProcessSpawner;
    /// Returns the clock capability.
    fn clock(&self) -> &dyn LocalClock;
    /// Returns the temporary-artifact capability.
    fn temporary_artifacts(&self) -> &dyn LocalTemporaryArtifactStore;
}

/// Process spawner for platforms that cannot execute native processes.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableProcessSpawner;

impl ProcessSpawner for UnavailableProcessSpawner {
    fn spawn(
        &self,
        _command: ProcessCommand,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RunningProcess>, ProcessError>> {
        Box::pin(async {
            Err(ProcessError::CapabilityUnavailable(
                CapabilityUnavailable::new(
                    "process_spawn",
                    "native process execution is not supported by this environment",
                ),
            ))
        })
    }
}

/// Local process spawner for platforms that cannot execute native processes.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalUnavailableProcessSpawner;

impl LocalProcessSpawner for LocalUnavailableProcessSpawner {
    fn spawn(
        &self,
        _command: ProcessCommand,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalRunningProcess>, ProcessError>> {
        Box::pin(async {
            Err(ProcessError::CapabilityUnavailable(
                CapabilityUnavailable::new(
                    "process_spawn",
                    "native process execution is not supported by this environment",
                ),
            ))
        })
    }
}
