//! Tokio-backed native environment capabilities.

use async_stream::stream;
use bytes::Bytes;
use pi_agent_env::{
    AgentEnvironment, AgentFileSystem, AgentPath, ArtifactError, ArtifactRef, CanonicalPath, Clock,
    ClockError, EditResult, FileReadResult, FileSystemError, FileWriteResult, ProcessCommand,
    ProcessError, ProcessEvent, ProcessEventStream, ProcessExitStatus, ProcessOutcome,
    ProcessSpawner, ProcessTermination, ReadLimits, RunningProcess, TemporaryArtifactRequest,
    TemporaryArtifactStore, TerminationPolicy, TerminationSignal,
};
use pi_ai::{CancellationToken, SendBoxFuture, Timestamp};
use std::{
    collections::VecDeque,
    ffi::OsStr,
    io,
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    time::{Instant, sleep_until},
};

const PROCESS_CHUNK_BYTES: usize = 8 * 1024;
const TEMP_CREATE_ATTEMPTS: u32 = 32;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Tokio-backed filesystem rooted at one configured working directory.
#[derive(Clone, Debug)]
pub struct TokioAgentFileSystem {
    cwd: Arc<PathBuf>,
}

impl TokioAgentFileSystem {
    /// Creates a filesystem capability. A relative working directory is made
    /// absolute against the host's current directory without requiring it to
    /// exist.
    pub fn new(cwd: impl Into<PathBuf>) -> Result<Self, FileSystemError> {
        let cwd = cwd.into();
        let absolute = if cwd.is_absolute() {
            cwd
        } else {
            std::env::current_dir()
                .map_err(|error| FileSystemError::Other {
                    path: None,
                    message: error.to_string(),
                })?
                .join(cwd)
        };
        Ok(Self {
            cwd: Arc::new(normalize_lexically(&absolute)),
        })
    }

    /// Returns the configured absolute working directory.
    pub fn current_dir(&self) -> &Path {
        &self.cwd
    }

    fn resolve(&self, path: &AgentPath) -> Result<AgentPath, FileSystemError> {
        let expanded =
            expand_portable_path(path.as_path()).map_err(|message| FileSystemError::Invalid {
                path: path.clone(),
                message,
            })?;
        let absolute = if expanded.is_absolute() {
            expanded
        } else {
            self.cwd.join(expanded)
        };
        Ok(AgentPath::new(normalize_lexically(&absolute)))
    }

    async fn atomic_write(
        &self,
        destination: &AgentPath,
        data: &[u8],
        permissions: Option<std::fs::Permissions>,
        cancellation: &CancellationToken,
    ) -> Result<(), FileSystemError> {
        let destination_path = destination.as_path();
        let parent = destination_path
            .parent()
            .unwrap_or_else(|| Path::new(std::path::MAIN_SEPARATOR_STR));
        let basename = destination_path
            .file_name()
            .unwrap_or_else(|| OsStr::new("file"))
            .to_string_lossy();

        let mut temporary = None;
        let mut file = None;
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".{basename}.pi-agent-{}-{sequence}.tmp",
                std::process::id()
            ));
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&candidate)
                .await
            {
                Ok(opened) => {
                    temporary = Some(candidate);
                    file = Some(opened);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(map_file_error(destination.clone(), error)),
            }
        }
        let temporary = temporary.ok_or_else(|| FileSystemError::Other {
            path: Some(destination.clone()),
            message: "could not allocate an atomic replacement file".into(),
        })?;
        let mut file = file.expect("temporary path and file are assigned together");

        let write_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(FileSystemError::Cancelled {
                path: destination.clone(),
            }),
            result = async {
                file.write_all(data)
                    .await
                    .map_err(|error| map_file_error(destination.clone(), error))?;
                file.flush()
                    .await
                    .map_err(|error| map_file_error(destination.clone(), error))?;
                if let Some(permissions) = permissions {
                    file.set_permissions(permissions)
                        .await
                        .map_err(|error| map_file_error(destination.clone(), error))?;
                }
                drop(file);
                cancellation.check().map_err(|_| FileSystemError::Cancelled {
                    path: destination.clone(),
                })?;
                fs::rename(&temporary, destination_path)
                    .await
                    .map_err(|error| map_file_error(destination.clone(), error))
            } => result,
        };

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary).await;
        }
        write_result
    }
}

impl AgentFileSystem for TokioAgentFileSystem {
    fn canonicalize(
        &self,
        path: &AgentPath,
    ) -> SendBoxFuture<'_, Result<CanonicalPath, FileSystemError>> {
        let resolved = self.resolve(path);
        Box::pin(async move {
            let resolved = resolved?;
            fs::canonicalize(resolved.as_path())
                .await
                .map(CanonicalPath::new)
                .map_err(|error| map_file_error(resolved, error))
        })
    }

    fn read(
        &self,
        path: &AgentPath,
        limits: ReadLimits,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileReadResult, FileSystemError>> {
        let resolved = self.resolve(path);
        Box::pin(async move {
            let resolved = resolved?;
            cancellation
                .check()
                .map_err(|_| FileSystemError::Cancelled {
                    path: resolved.clone(),
                })?;

            let metadata = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(FileSystemError::Cancelled {
                    path: resolved.clone(),
                }),
                result = fs::metadata(resolved.as_path()) => result,
            }
            .map_err(|error| map_file_error(resolved.clone(), error))?;
            if metadata.is_dir() {
                return Err(FileSystemError::IsDirectory {
                    path: resolved,
                    message: "cannot read a directory as a file".into(),
                });
            }

            let (mut data, observed_bytes) = if let Some(max_bytes) = limits.max_bytes {
                let file = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(FileSystemError::Cancelled {
                        path: resolved.clone(),
                    }),
                    result = File::open(resolved.as_path()) => result,
                }
                .map_err(|error| map_file_error(resolved.clone(), error))?;
                let read_limit = max_bytes.saturating_add(1);
                let mut reader = file.take(read_limit);
                let mut bytes = Vec::new();
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(FileSystemError::Cancelled {
                        path: resolved.clone(),
                    }),
                    result = reader.read_to_end(&mut bytes) => result,
                }
                .map_err(|error| map_file_error(resolved.clone(), error))?;
                let observed_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                let retained = usize::try_from(max_bytes).unwrap_or(usize::MAX);
                bytes.truncate(retained);
                (bytes, observed_bytes)
            } else {
                let bytes = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(FileSystemError::Cancelled {
                        path: resolved.clone(),
                    }),
                    result = fs::read(resolved.as_path()) => result,
                }
                .map_err(|error| map_file_error(resolved.clone(), error))?;
                let observed_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                (bytes, observed_bytes)
            };
            data.shrink_to_fit();
            let total_bytes = metadata.len().max(observed_bytes);
            Ok(FileReadResult {
                path: resolved,
                data: Bytes::from(data),
                total_bytes,
                truncated: limits
                    .max_bytes
                    .is_some_and(|limit| observed_bytes > limit || metadata.len() > limit),
            })
        })
    }

    fn write(
        &self,
        path: &AgentPath,
        data: Bytes,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<FileWriteResult, FileSystemError>> {
        let resolved = self.resolve(path);
        Box::pin(async move {
            let resolved = resolved?;
            cancellation
                .check()
                .map_err(|_| FileSystemError::Cancelled {
                    path: resolved.clone(),
                })?;
            if let Some(parent) = resolved.as_path().parent() {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(FileSystemError::Cancelled {
                        path: resolved.clone(),
                    }),
                    result = fs::create_dir_all(parent) => result,
                }
                .map_err(|error| map_file_error(resolved.clone(), error))?;
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(FileSystemError::Cancelled {
                    path: resolved.clone(),
                }),
                result = fs::write(resolved.as_path(), &data) => result,
            }
            .map_err(|error| map_file_error(resolved.clone(), error))?;
            Ok(FileWriteResult {
                path: resolved,
                bytes_written: u64::try_from(data.len()).unwrap_or(u64::MAX),
            })
        })
    }

    fn replace_exact(
        &self,
        path: &AgentPath,
        expected: &str,
        replacement: &str,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<EditResult, FileSystemError>> {
        let resolved = self.resolve(path);
        let expected = expected.to_owned();
        let replacement = replacement.to_owned();
        Box::pin(async move {
            let resolved = resolved?;
            if expected.is_empty() {
                return Err(FileSystemError::Invalid {
                    path: resolved,
                    message: "exact replacement text must not be empty".into(),
                });
            }
            if expected == replacement {
                return Err(FileSystemError::NoOpReplacement { path: resolved });
            }
            let read = self
                .read(&resolved, ReadLimits::default(), cancellation.clone())
                .await?;
            let text = std::str::from_utf8(&read.data).map_err(|_| FileSystemError::Invalid {
                path: resolved.clone(),
                message: "exact replacement requires a UTF-8 file".into(),
            })?;
            let matches = u64::try_from(text.matches(&expected).count()).unwrap_or(u64::MAX);
            match matches {
                0 => return Err(FileSystemError::ExactMatchNotFound { path: resolved }),
                1 => {}
                _ => {
                    return Err(FileSystemError::MultipleExactMatches {
                        path: resolved,
                        matches,
                    });
                }
            }

            let replacement_data = text.replacen(&expected, &replacement, 1).into_bytes();
            let permissions = fs::metadata(resolved.as_path())
                .await
                .map_err(|error| map_file_error(resolved.clone(), error))?
                .permissions();
            self.atomic_write(
                &resolved,
                &replacement_data,
                Some(permissions),
                &cancellation,
            )
            .await?;
            Ok(EditResult {
                path: resolved,
                replacements: 1,
                bytes_written: u64::try_from(replacement_data.len()).unwrap_or(u64::MAX),
            })
        })
    }
}

/// Tokio wall clock and cancellable timer capability.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioClock;

impl Clock for TokioClock {
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
    }

    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), ClockError>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(ClockError::Cancelled),
                () = tokio::time::sleep(duration) => Ok(()),
            }
        })
    }
}

/// Tokio-backed temporary artifact store.
#[derive(Clone, Debug)]
pub struct TokioTemporaryArtifactStore {
    root: Arc<PathBuf>,
    filesystem: Arc<TokioAgentFileSystem>,
}

impl TokioTemporaryArtifactStore {
    /// Creates a store rooted at `root` and using `filesystem` for writes.
    pub fn new(root: impl Into<PathBuf>, filesystem: Arc<TokioAgentFileSystem>) -> Self {
        Self {
            root: Arc::new(root.into()),
            filesystem,
        }
    }
}

impl TemporaryArtifactStore for TokioTemporaryArtifactStore {
    fn create(
        &self,
        request: TemporaryArtifactRequest,
        data: Bytes,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<ArtifactRef, ArtifactError>> {
        Box::pin(async move {
            let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let filename = format!(
                "{}{}-{sequence}{}",
                request.prefix,
                std::process::id(),
                request.suffix
            );
            let path = AgentPath::new(self.root.join(filename));
            self.filesystem
                .write(&path, data, cancellation.clone())
                .await
                .map_err(ArtifactError::FileSystem)?;
            let canonical = self
                .filesystem
                .canonicalize(&path)
                .await
                .map_err(ArtifactError::FileSystem)?;
            Ok(ArtifactRef { path: canonical })
        })
    }

    fn remove(
        &self,
        artifact: &ArtifactRef,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), ArtifactError>> {
        let path = AgentPath::new(artifact.path.as_path());
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(ArtifactError::FileSystem(
                    FileSystemError::Cancelled { path: path.clone() },
                )),
                result = fs::remove_file(path.as_path()) => result
                    .map_err(|error| ArtifactError::FileSystem(map_file_error(path, error))),
            }
        })
    }
}

/// Tokio native process spawner.
#[derive(Clone, Debug)]
pub struct TokioProcessSpawner {
    cwd: AgentPath,
}

impl TokioProcessSpawner {
    /// Creates a process spawner whose default working directory is `cwd`.
    pub fn new(cwd: impl Into<AgentPath>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl ProcessSpawner for TokioProcessSpawner {
    fn spawn(
        &self,
        command: ProcessCommand,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RunningProcess>, ProcessError>> {
        let default_cwd = self.cwd.clone();
        Box::pin(async move {
            cancellation.check().map_err(|_| ProcessError::Cancelled)?;
            let ProcessCommand {
                program,
                arguments,
                current_dir,
                environment,
                inherit_environment,
                stdin: stdin_input,
                cancellation_policy,
            } = command;
            if program.is_empty() {
                return Err(ProcessError::Spawn {
                    message: "program must not be empty".into(),
                });
            }

            let current_dir = match current_dir.as_ref() {
                Some(path) if path.as_path().is_absolute() => path.clone(),
                Some(path) => AgentPath::new(default_cwd.as_path().join(path.as_path())),
                None => default_cwd.clone(),
            };
            let mut native = Command::new(&program);
            native
                .args(&arguments)
                .current_dir(current_dir.as_path())
                .stdin(if stdin_input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            if !inherit_environment {
                native.env_clear();
            }
            native.envs(&environment);

            configure_process_group(&mut native);
            let mut child = native.spawn().map_err(|error| ProcessError::Spawn {
                message: error.to_string(),
            })?;
            let pid = child.id();
            let process_tree = match NativeProcessTree::new(&child) {
                Ok(process_tree) => process_tree,
                Err(error) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(ProcessError::Spawn {
                        message: error.to_string(),
                    });
                }
            };
            if cancellation.is_cancelled() {
                force_kill(
                    &mut child,
                    pid,
                    cancellation_policy.terminate_process_tree,
                    &process_tree,
                );
                let _ = child.wait().await;
                return Err(ProcessError::Cancelled);
            }
            if let Err(error) = process_tree.resume(&child) {
                force_kill(
                    &mut child,
                    pid,
                    cancellation_policy.terminate_process_tree,
                    &process_tree,
                );
                let _ = child.wait().await;
                return Err(ProcessError::Spawn {
                    message: error.to_string(),
                });
            }

            let stdin = child.stdin.take();
            if stdin_input.is_some() && stdin.is_none() {
                force_kill(
                    &mut child,
                    pid,
                    cancellation_policy.terminate_process_tree,
                    &process_tree,
                );
                let _ = child.wait().await;
                return Err(ProcessError::Io {
                    message: "spawned process did not expose piped stdin".into(),
                });
            }
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            Ok(Box::new(TokioRunningProcess {
                child,
                pipes: ProcessPipes {
                    stdin,
                    stdout,
                    stderr,
                },
                stdin_input,
                stdin_offset: 0,
                pid,
                cancellation,
                cancellation_policy,
                process_tree,
                pending_events: VecDeque::new(),
                outcome: None,
                terminal_emitted: false,
            }) as Box<dyn RunningProcess>)
        })
    }
}

struct ProcessPipes {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

struct TokioRunningProcess {
    child: Child,
    pipes: ProcessPipes,
    stdin_input: Option<Bytes>,
    stdin_offset: usize,
    pid: Option<u32>,
    cancellation: CancellationToken,
    cancellation_policy: TerminationPolicy,
    process_tree: NativeProcessTree,
    pending_events: VecDeque<ProcessEvent>,
    outcome: Option<ProcessOutcome>,
    terminal_emitted: bool,
}

impl RunningProcess for TokioRunningProcess {
    fn events(&mut self) -> ProcessEventStream<'_> {
        Box::pin(stream! {
            if self.terminal_emitted {
                return;
            }
            while let Some(event) = self.pending_events.pop_front() {
                yield Ok(event);
            }
            if let Some(outcome) = self.outcome.clone() {
                self.terminal_emitted = true;
                yield Ok(ProcessEvent::Exited(outcome));
                return;
            }

            let mut stdout_buffer = vec![0_u8; PROCESS_CHUNK_BYTES];
            let mut stderr_buffer = vec![0_u8; PROCESS_CHUNK_BYTES];
            let mut exit_status = None;
            let mut stdio_deadline = None;

            loop {
                if exit_status.is_some()
                    && self.pipes.stdin.is_none()
                    && self.pipes.stdout.is_none()
                    && self.pipes.stderr.is_none()
                {
                    break;
                }
                let cancellation = self.cancellation.clone();
                tokio::select! {
                    _ = cancellation.cancelled(), if exit_status.is_none() => {
                        match terminate_running(
                            &mut self.child,
                            &mut self.pipes,
                            self.pid,
                            &self.cancellation_policy,
                            ProcessTermination::Cancelled,
                            &self.process_tree,
                        ).await {
                            Ok(terminated) => {
                                for event in terminated.output {
                                    yield Ok(event);
                                }
                                self.outcome = Some(terminated.outcome.clone());
                                self.terminal_emitted = true;
                                yield Ok(ProcessEvent::Exited(terminated.outcome));
                            }
                            Err(error) => yield Err(error),
                        }
                        return;
                    }
                    result = self.child.wait(), if exit_status.is_none() => {
                        match result {
                            Ok(status) => {
                                exit_status = Some(status);
                                stdio_deadline = Some(Instant::now() + self.cancellation_policy.stdio_grace_period);
                            }
                            Err(error) => {
                                let cleanup = kill_and_join_after_io_failure(
                                    &mut self.child,
                                    &mut self.pipes,
                                    self.pid,
                                    &self.cancellation_policy,
                                    &self.process_tree,
                                ).await;
                                self.terminal_emitted = true;
                                match cleanup {
                                    Ok(outcome) => {
                                        self.outcome = Some(outcome);
                                        yield Err(ProcessError::Io { message: error.to_string() });
                                    }
                                    Err(cleanup_error) => {
                                        yield Err(ProcessError::Io {
                                            message: format!(
                                                "could not observe process exit: {error}; cleanup failed: {cleanup_error}"
                                            ),
                                        });
                                    }
                                }
                                return;
                            }
                        }
                    }
                    () = wait_for_deadline(stdio_deadline), if stdio_deadline.is_some() => {
                        self.pipes.stdin = None;
                        self.pipes.stdout = None;
                        self.pipes.stderr = None;
                        break;
                    }
                    result = pump_stdin_once(
                        &mut self.pipes.stdin,
                        self.stdin_input.as_ref(),
                        &mut self.stdin_offset,
                    ), if self.pipes.stdin.is_some() => {
                        if let Err(error) = result {
                            let cleanup = kill_and_join_after_io_failure(
                                &mut self.child,
                                &mut self.pipes,
                                self.pid,
                                &self.cancellation_policy,
                                &self.process_tree,
                            ).await;
                            self.terminal_emitted = true;
                            match cleanup {
                                Ok(outcome) => {
                                    self.outcome = Some(outcome);
                                    yield Err(ProcessError::Io {
                                        message: format!("could not write process stdin: {error}"),
                                    });
                                }
                                Err(cleanup_error) => {
                                    yield Err(ProcessError::Io {
                                        message: format!(
                                            "could not write process stdin: {error}; cleanup failed: {cleanup_error}"
                                        ),
                                    });
                                }
                            }
                            return;
                        }
                    }
                    result = read_optional(&mut self.pipes.stdout, &mut stdout_buffer), if self.pipes.stdout.is_some() => {
                        match result {
                            Ok(0) => self.pipes.stdout = None,
                            Ok(count) => {
                                if exit_status.is_some() {
                                    stdio_deadline = Some(
                                        Instant::now() + self.cancellation_policy.stdio_grace_period,
                                    );
                                }
                                yield Ok(ProcessEvent::Stdout(Bytes::copy_from_slice(&stdout_buffer[..count])));
                            }
                            Err(error) => {
                                let cleanup = kill_and_join_after_io_failure(
                                    &mut self.child,
                                    &mut self.pipes,
                                    self.pid,
                                    &self.cancellation_policy,
                                    &self.process_tree,
                                ).await;
                                self.terminal_emitted = true;
                                match cleanup {
                                    Ok(outcome) => {
                                        self.outcome = Some(outcome);
                                        yield Err(ProcessError::Io {
                                            message: format!("could not read process stdout: {error}"),
                                        });
                                    }
                                    Err(cleanup_error) => {
                                        yield Err(ProcessError::Io {
                                            message: format!(
                                                "could not read process stdout: {error}; cleanup failed: {cleanup_error}"
                                            ),
                                        });
                                    }
                                }
                                return;
                            }
                        }
                    }
                    result = read_optional(&mut self.pipes.stderr, &mut stderr_buffer), if self.pipes.stderr.is_some() => {
                        match result {
                            Ok(0) => self.pipes.stderr = None,
                            Ok(count) => {
                                if exit_status.is_some() {
                                    stdio_deadline = Some(
                                        Instant::now() + self.cancellation_policy.stdio_grace_period,
                                    );
                                }
                                yield Ok(ProcessEvent::Stderr(Bytes::copy_from_slice(&stderr_buffer[..count])));
                            }
                            Err(error) => {
                                let cleanup = kill_and_join_after_io_failure(
                                    &mut self.child,
                                    &mut self.pipes,
                                    self.pid,
                                    &self.cancellation_policy,
                                    &self.process_tree,
                                ).await;
                                self.terminal_emitted = true;
                                match cleanup {
                                    Ok(outcome) => {
                                        self.outcome = Some(outcome);
                                        yield Err(ProcessError::Io {
                                            message: format!("could not read process stderr: {error}"),
                                        });
                                    }
                                    Err(cleanup_error) => {
                                        yield Err(ProcessError::Io {
                                            message: format!(
                                                "could not read process stderr: {error}; cleanup failed: {cleanup_error}"
                                            ),
                                        });
                                    }
                                }
                                return;
                            }
                        }
                    }
                }
            }

            let status = exit_status.expect("event loop exits only after child status");
            if !self.cancellation_policy.terminate_process_tree
                && let Err(error) = self.process_tree.preserve_descendants()
            {
                self.terminal_emitted = true;
                yield Err(process_io_error(error));
                return;
            }
            let outcome = ProcessOutcome {
                status: portable_exit_status(status),
                termination: ProcessTermination::Exited,
            };
            self.outcome = Some(outcome.clone());
            self.terminal_emitted = true;
            yield Ok(ProcessEvent::Exited(outcome));
        })
    }

    fn terminate(
        &mut self,
        policy: TerminationPolicy,
    ) -> SendBoxFuture<'_, Result<ProcessOutcome, ProcessError>> {
        Box::pin(async move {
            if let Some(outcome) = &self.outcome {
                return Ok(outcome.clone());
            }
            if !policy.terminate_process_tree {
                self.cancellation_policy.terminate_process_tree = false;
            }
            let terminated = terminate_running(
                &mut self.child,
                &mut self.pipes,
                self.pid,
                &policy,
                if policy.graceful_signal == TerminationSignal::Kill {
                    ProcessTermination::Forced
                } else {
                    ProcessTermination::Graceful
                },
                &self.process_tree,
            )
            .await?;
            self.pending_events.extend(terminated.output);
            let outcome = terminated.outcome;
            self.outcome = Some(outcome.clone());
            Ok(outcome)
        })
    }
}

impl Drop for TokioRunningProcess {
    fn drop(&mut self) {
        if self.outcome.is_none() {
            force_kill(
                &mut self.child,
                self.pid,
                self.cancellation_policy.terminate_process_tree,
                &self.process_tree,
            );
        }
    }
}

/// Complete standard Tokio environment.
#[derive(Clone, Debug)]
pub struct TokioAgentEnvironment {
    filesystem: Arc<TokioAgentFileSystem>,
    processes: TokioProcessSpawner,
    clock: TokioClock,
    artifacts: TokioTemporaryArtifactStore,
}

impl TokioAgentEnvironment {
    /// Creates a native environment rooted at `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> Result<Self, FileSystemError> {
        let filesystem = Arc::new(TokioAgentFileSystem::new(cwd)?);
        let processes = TokioProcessSpawner::new(AgentPath::new(filesystem.current_dir()));
        let artifacts = TokioTemporaryArtifactStore::new(
            std::env::temp_dir().join("pi-agent-artifacts"),
            filesystem.clone(),
        );
        Ok(Self {
            filesystem,
            processes,
            clock: TokioClock,
            artifacts,
        })
    }
}

impl AgentEnvironment for TokioAgentEnvironment {
    fn filesystem(&self) -> &dyn AgentFileSystem {
        self.filesystem.as_ref()
    }

    fn processes(&self) -> &dyn ProcessSpawner {
        &self.processes
    }

    fn clock(&self) -> &dyn Clock {
        &self.clock
    }

    fn temporary_artifacts(&self) -> &dyn TemporaryArtifactStore {
        &self.artifacts
    }
}

async fn terminate_running(
    child: &mut Child,
    pipes: &mut ProcessPipes,
    pid: Option<u32>,
    policy: &TerminationPolicy,
    requested_termination: ProcessTermination,
    process_tree: &NativeProcessTree,
) -> Result<TerminatedProcess, ProcessError> {
    pipes.stdin = None;
    let mut output = VecDeque::new();
    if let Some(status) = child.try_wait().map_err(process_io_error)? {
        if !policy.terminate_process_tree {
            process_tree
                .preserve_descendants()
                .map_err(process_io_error)?;
        }
        drain_stdio(
            &mut pipes.stdout,
            &mut pipes.stderr,
            policy.stdio_grace_period,
            &mut output,
        )
        .await?;
        return Ok(TerminatedProcess {
            outcome: ProcessOutcome {
                status: portable_exit_status(status),
                termination: ProcessTermination::Exited,
            },
            output,
        });
    }

    let graceful_delivery = send_signal(
        child,
        pid,
        policy.graceful_signal,
        policy.terminate_process_tree,
        process_tree,
    )
    .map_err(process_io_error)?;
    if let Some(status) = wait_and_drain(
        child,
        &mut pipes.stdout,
        &mut pipes.stderr,
        policy.graceful_timeout,
        &mut output,
    )
    .await?
    {
        drain_stdio(
            &mut pipes.stdout,
            &mut pipes.stderr,
            policy.stdio_grace_period,
            &mut output,
        )
        .await?;
        return Ok(TerminatedProcess {
            outcome: ProcessOutcome {
                status: portable_exit_status(status),
                termination: termination_after_delivery(requested_termination, graceful_delivery),
            },
            output,
        });
    }

    send_signal(
        child,
        pid,
        TerminationSignal::Kill,
        policy.terminate_process_tree,
        process_tree,
    )
    .map_err(process_io_error)?;
    let Some(status) = wait_and_drain(
        child,
        &mut pipes.stdout,
        &mut pipes.stderr,
        policy.forced_timeout,
        &mut output,
    )
    .await?
    else {
        return Err(ProcessError::TerminationTimeout { forced: true });
    };
    drain_stdio(
        &mut pipes.stdout,
        &mut pipes.stderr,
        policy.stdio_grace_period,
        &mut output,
    )
    .await?;
    Ok(TerminatedProcess {
        outcome: ProcessOutcome {
            status: portable_exit_status(status),
            termination: if requested_termination == ProcessTermination::Cancelled {
                ProcessTermination::Cancelled
            } else {
                ProcessTermination::Forced
            },
        },
        output,
    })
}

struct TerminatedProcess {
    outcome: ProcessOutcome,
    output: VecDeque<ProcessEvent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignalDelivery {
    Requested,
    Forced,
}

fn termination_after_delivery(
    requested: ProcessTermination,
    delivery: SignalDelivery,
) -> ProcessTermination {
    if requested == ProcessTermination::Cancelled {
        ProcessTermination::Cancelled
    } else if delivery == SignalDelivery::Forced {
        ProcessTermination::Forced
    } else {
        requested
    }
}

async fn pump_stdin_once(
    stdin: &mut Option<ChildStdin>,
    input: Option<&Bytes>,
    offset: &mut usize,
) -> io::Result<()> {
    let Some(stdin_pipe) = stdin.as_mut() else {
        return Ok(());
    };
    let input = input.ok_or_else(|| io::Error::other("piped stdin has no input payload"))?;
    if *offset < input.len() {
        let end = input.len().min(offset.saturating_add(PROCESS_CHUNK_BYTES));
        let written = stdin_pipe.write(&input[*offset..end]).await?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "process stdin accepted zero bytes",
            ));
        }
        *offset = offset.saturating_add(written);
        return Ok(());
    }

    stdin_pipe.shutdown().await?;
    *stdin = None;
    Ok(())
}

async fn kill_and_join_after_io_failure(
    child: &mut Child,
    pipes: &mut ProcessPipes,
    pid: Option<u32>,
    policy: &TerminationPolicy,
    process_tree: &NativeProcessTree,
) -> Result<ProcessOutcome, ProcessError> {
    pipes.stdin = None;
    let mut discarded_output = VecDeque::new();
    send_signal(
        child,
        pid,
        TerminationSignal::Kill,
        policy.terminate_process_tree,
        process_tree,
    )
    .map_err(process_io_error)?;
    let Some(status) = wait_and_drain(
        child,
        &mut pipes.stdout,
        &mut pipes.stderr,
        policy.forced_timeout,
        &mut discarded_output,
    )
    .await?
    else {
        return Err(ProcessError::TerminationTimeout { forced: true });
    };
    drain_stdio(
        &mut pipes.stdout,
        &mut pipes.stderr,
        policy.stdio_grace_period,
        &mut discarded_output,
    )
    .await?;
    Ok(ProcessOutcome {
        status: portable_exit_status(status),
        termination: ProcessTermination::Forced,
    })
}

async fn wait_and_drain(
    child: &mut Child,
    stdout: &mut Option<ChildStdout>,
    stderr: &mut Option<ChildStderr>,
    timeout: Duration,
    output: &mut VecDeque<ProcessEvent>,
) -> Result<Option<ExitStatus>, ProcessError> {
    let deadline = Instant::now() + timeout;
    let mut stdout_buffer = vec![0_u8; PROCESS_CHUNK_BYTES];
    let mut stderr_buffer = vec![0_u8; PROCESS_CHUNK_BYTES];
    loop {
        tokio::select! {
            biased;
            result = child.wait() => return result.map(Some).map_err(process_io_error),
            () = sleep_until(deadline) => return Ok(None),
            result = read_optional(stdout, &mut stdout_buffer), if stdout.is_some() => {
                match result {
                    Ok(0) => *stdout = None,
                    Ok(count) => output.push_back(ProcessEvent::Stdout(
                        Bytes::copy_from_slice(&stdout_buffer[..count]),
                    )),
                    Err(error) => return Err(process_io_error(error)),
                }
            }
            result = read_optional(stderr, &mut stderr_buffer), if stderr.is_some() => {
                match result {
                    Ok(0) => *stderr = None,
                    Ok(count) => output.push_back(ProcessEvent::Stderr(
                        Bytes::copy_from_slice(&stderr_buffer[..count]),
                    )),
                    Err(error) => return Err(process_io_error(error)),
                }
            }
        }
    }
}

async fn drain_stdio(
    stdout: &mut Option<ChildStdout>,
    stderr: &mut Option<ChildStderr>,
    grace: Duration,
    output: &mut VecDeque<ProcessEvent>,
) -> Result<(), ProcessError> {
    let mut deadline = Instant::now() + grace;
    let mut stdout_buffer = vec![0_u8; PROCESS_CHUNK_BYTES];
    let mut stderr_buffer = vec![0_u8; PROCESS_CHUNK_BYTES];
    while stdout.is_some() || stderr.is_some() {
        tokio::select! {
            biased;
            () = sleep_until(deadline) => {
                *stdout = None;
                *stderr = None;
            }
            result = read_optional(stdout, &mut stdout_buffer), if stdout.is_some() => {
                match result {
                    Ok(0) => *stdout = None,
                    Ok(count) => {
                        output.push_back(ProcessEvent::Stdout(
                            Bytes::copy_from_slice(&stdout_buffer[..count]),
                        ));
                        deadline = Instant::now() + grace;
                    }
                    Err(error) => return Err(process_io_error(error)),
                }
            }
            result = read_optional(stderr, &mut stderr_buffer), if stderr.is_some() => {
                match result {
                    Ok(0) => *stderr = None,
                    Ok(count) => {
                        output.push_back(ProcessEvent::Stderr(
                            Bytes::copy_from_slice(&stderr_buffer[..count]),
                        ));
                        deadline = Instant::now() + grace;
                    }
                    Err(error) => return Err(process_io_error(error)),
                }
            }
        }
    }
    Ok(())
}

async fn read_optional<R>(reader: &mut Option<R>, buffer: &mut [u8]) -> io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    match reader {
        Some(reader) => reader.read(buffer).await,
        None => Ok(0),
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn process_io_error(error: io::Error) -> ProcessError {
    ProcessError::Io {
        message: error.to_string(),
    }
}

fn map_file_error(path: AgentPath, error: io::Error) -> FileSystemError {
    let message = error.to_string();
    match error.kind() {
        io::ErrorKind::NotFound => FileSystemError::NotFound { path, message },
        io::ErrorKind::PermissionDenied => FileSystemError::PermissionDenied { path, message },
        io::ErrorKind::NotADirectory => FileSystemError::NotDirectory { path, message },
        io::ErrorKind::IsADirectory => FileSystemError::IsDirectory { path, message },
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
            FileSystemError::Invalid { path, message }
        }
        _ => FileSystemError::Other {
            path: Some(path),
            message,
        },
    }
}

fn expand_portable_path(path: &Path) -> Result<PathBuf, String> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") || cfg!(windows) && text.starts_with("~\\") {
        let home = home_directory().ok_or_else(|| "home directory is unavailable".to_owned())?;
        if text == "~" {
            return Ok(home);
        }
        return Ok(home.join(&text[2..]));
    }
    if text.starts_with("file://") {
        let url = url::Url::parse(&text).map_err(|error| error.to_string())?;
        return url
            .to_file_path()
            .map_err(|()| "file URL does not identify a local path".into());
    }
    Ok(path.to_path_buf())
}

fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| {
                let drive = std::env::var_os("HOMEDRIVE")?;
                let path = std::env::var_os("HOMEPATH")?;
                Some(PathBuf::from(drive).join(path))
            })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn portable_exit_status(status: ExitStatus) -> ProcessExitStatus {
    ProcessExitStatus {
        code: status.code(),
        signal: exit_signal(&status),
        success: status.success(),
    }
}

#[cfg(unix)]
fn exit_signal(status: &ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| format!("SIG{signal}"))
}

#[cfg(not(unix))]
fn exit_signal(_status: &ExitStatus) -> Option<String> {
    None
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED};

    command
        .as_std_mut()
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
}

#[cfg(not(any(unix, windows)))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
struct NativeProcessTree;

#[cfg(unix)]
impl NativeProcessTree {
    fn new(_child: &Child) -> io::Result<Self> {
        Ok(Self)
    }

    fn resume(&self, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    fn preserve_descendants(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
struct NativeProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
    kill_on_close: std::sync::atomic::AtomicBool,
}

#[cfg(windows)]
// SAFETY: a Windows job HANDLE may be used and closed from any process thread.
unsafe impl Send for NativeProcessTree {}

#[cfg(windows)]
// SAFETY: Windows Job Object operations are thread-safe, and the handle is
// closed only when the owning `NativeProcessTree` is dropped.
unsafe impl Sync for NativeProcessTree {}

#[cfg(windows)]
impl NativeProcessTree {
    fn new(child: &Child) -> io::Result<Self> {
        use std::{mem::size_of, ptr};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        // SAFETY: null security/name pointers request an unnamed job with
        // default security. The returned owned HANDLE is closed in Drop.
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` has the exact structure and length required by the
        // selected information class, and `job` is a live owned handle.
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("Windows job information size fits u32"),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            // SAFETY: `job` is a live owned handle not used after this call.
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(job);
            }
            return Err(error);
        }
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("spawned process has no handle"))?
            as windows_sys::Win32::Foundation::HANDLE;
        // SAFETY: both handles are live and owned by this process.
        if unsafe { AssignProcessToJobObject(job, process) } == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: `job` is a live owned handle not used after this call.
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(job);
            }
            return Err(error);
        }
        Ok(Self {
            job,
            kill_on_close: std::sync::atomic::AtomicBool::new(true),
        })
    }

    fn resume(&self, child: &Child) -> io::Result<()> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("spawned process has no identifier"))?;
        resume_initial_thread(pid)
    }

    fn terminate(&self) -> io::Result<()> {
        // SAFETY: `self.job` remains live for the duration of the call.
        if unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn preserve_descendants(&self) -> io::Result<()> {
        use std::mem::size_of;
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        if !self.kill_on_close.load(Ordering::Acquire) {
            return Ok(());
        }
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: `limits` has the exact structure and length required by the
        // selected information class, and `self.job` remains live.
        if unsafe {
            SetInformationJobObject(
                self.job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("Windows job information size fits u32"),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        self.kill_on_close.store(false, Ordering::Release);
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for NativeProcessTree {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of the job handle.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct NativeProcessTree;

#[cfg(not(any(unix, windows)))]
impl NativeProcessTree {
    fn new(_child: &Child) -> io::Result<Self> {
        Ok(Self)
    }

    fn resume(&self, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    fn preserve_descendants(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
fn resume_initial_thread(pid: u32) -> io::Result<()> {
    use std::mem::size_of;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    // SAFETY: the snapshot request has no pointer arguments and returns an
    // owned handle closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    let result = (|| {
        let mut entry = THREADENTRY32 {
            dwSize: u32::try_from(size_of::<THREADENTRY32>())
                .expect("Windows thread entry size fits u32"),
            ..THREADENTRY32::default()
        };
        // SAFETY: `snapshot` is live and `entry` points to writable storage
        // with the required size field initialized.
        if unsafe { Thread32First(snapshot, &raw mut entry) } == 0 {
            return Err(io::Error::last_os_error());
        }

        loop {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: the thread id came from the live system snapshot.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: `thread` is a live handle opened with suspend/resume
                // access. `u32::MAX` is the documented failure sentinel.
                let previous_count = unsafe { ResumeThread(thread) };
                // SAFETY: `thread` is an owned handle not used after closing.
                unsafe {
                    CloseHandle(thread);
                }
                return if previous_count == u32::MAX {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                };
            }

            // SAFETY: `snapshot` and `entry` remain valid as above. A false
            // result means either end-of-snapshot or an error; neither can
            // produce the requested process thread.
            if unsafe { Thread32Next(snapshot, &raw mut entry) } == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "could not find the suspended process thread",
                ));
            }
        }
    })();

    // SAFETY: `snapshot` is an owned handle and all users have completed.
    unsafe {
        CloseHandle(snapshot);
    }
    result
}

#[cfg(unix)]
fn send_signal(
    child: &mut Child,
    pid: Option<u32>,
    signal: TerminationSignal,
    tree: bool,
    _process_tree: &NativeProcessTree,
) -> io::Result<SignalDelivery> {
    let Some(pid) = pid else {
        child.start_kill()?;
        return Ok(SignalDelivery::Forced);
    };
    let signal = match signal {
        TerminationSignal::Terminate => libc::SIGTERM,
        TerminationSignal::Interrupt => libc::SIGINT,
        TerminationSignal::Kill => libc::SIGKILL,
    };
    let pid = i32::try_from(pid).map_err(|_| io::Error::other("process identifier overflow"))?;
    let target = if tree { -pid } else { pid };
    // SAFETY: `kill` is called with a process or process-group identifier
    // created by this spawner and a constant valid POSIX signal.
    let result = unsafe { libc::kill(target, signal) };
    if result == 0 {
        return Ok(SignalDelivery::Requested);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(SignalDelivery::Requested)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn send_signal(
    child: &mut Child,
    pid: Option<u32>,
    signal: TerminationSignal,
    tree: bool,
    process_tree: &NativeProcessTree,
) -> io::Result<SignalDelivery> {
    if !tree {
        process_tree.preserve_descendants()?;
        child.start_kill()?;
        return Ok(SignalDelivery::Forced);
    }
    if signal == TerminationSignal::Kill {
        process_tree.terminate()?;
        return Ok(SignalDelivery::Forced);
    }
    if let Some(pid) = pid {
        // Windows has no SIGTERM. A new process group receives CTRL_BREAK as
        // the graceful tree-wide analogue; failure is allowed to reach the
        // configured forced phase, which terminates the Job Object.
        // SAFETY: the event and process-group identifier are valid.
        unsafe {
            windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent(
                windows_sys::Win32::System::Console::CTRL_BREAK_EVENT,
                pid,
            );
        }
    }
    Ok(SignalDelivery::Requested)
}

#[cfg(not(any(unix, windows)))]
fn send_signal(
    child: &mut Child,
    _pid: Option<u32>,
    _signal: TerminationSignal,
    _tree: bool,
    _process_tree: &NativeProcessTree,
) -> io::Result<SignalDelivery> {
    child.start_kill()?;
    Ok(SignalDelivery::Forced)
}

fn force_kill(child: &mut Child, pid: Option<u32>, tree: bool, process_tree: &NativeProcessTree) {
    let _ = send_signal(child, pid, TerminationSignal::Kill, tree, process_tree);
}
