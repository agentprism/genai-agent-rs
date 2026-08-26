//! Executor-neutral reference-tool helpers from Architecture v2 part 2 §7.11.

use crate::{TruncationLimits, TruncationResult, TruncationStrategy, format_size, truncate};
use agentprism_ai::{
    CancellationToken, ContentBlockId, LocalBoxFuture, SendBoxFuture, ToolResultContent,
};
use agentprism_core::ToolOutput;
use agentprism_env::{
    AgentFileSystem, AgentPath, ArtifactError, ArtifactRef, CanonicalPath, Clock, ClockError,
    EditResult, FileSystemError, LocalAgentFileSystem, LocalClock, LocalProcessSpawner,
    LocalRunningProcess, LocalTemporaryArtifactStore, ProcessCommand, ProcessError, ProcessEvent,
    ProcessExitStatus, ProcessOutcome, ProcessSpawner, ProcessTermination, ReadLimits,
    RunningProcess, TemporaryArtifactRequest, TemporaryArtifactStore, TerminationPolicy,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use bytes::Bytes;
use dashmap::{DashMap, mapref::entry::Entry};
use futures_util::{StreamExt, future::Either, future::select, lock::Mutex as AsyncMutex};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    sync::{Arc, Mutex as SyncMutex, MutexGuard as SyncMutexGuard},
    time::Duration,
};
use unicode_normalization::UnicodeNormalization;

/// Persisted read-tool detail schema version.
pub const READ_TOOL_DETAILS_SCHEMA_VERSION: u32 = 1;

/// Persisted bash-tool detail schema version.
pub const BASH_TOOL_DETAILS_SCHEMA_VERSION: u32 = 1;

/// Pi's default model-visible line limit for file and shell output.
pub const DEFAULT_TOOL_MAX_LINES: usize = 2_000;

/// Pi's default model-visible UTF-8 byte limit for file and shell output.
pub const DEFAULT_TOOL_MAX_BYTES: usize = 50 * 1_024;

/// Optional bounds for the reference read operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReadToolRequest {
    /// One-based line offset. Omission starts at line one.
    pub offset: Option<usize>,
    /// Caller-selected maximum lines before ordinary tool truncation.
    pub limit: Option<usize>,
}

/// Persistable accounting returned when the reference read operation truncates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReadToolDetails {
    /// Native schema version.
    pub schema_version: u32,
    /// Complete truncation accounting.
    pub truncation: TruncationResult,
}

/// Successful image conversion performed by a host image processor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessedReadImage {
    /// Base64 image bytes without a data URL prefix.
    pub data: String,
    /// Media type of the processed image.
    pub mime_type: String,
    /// Ordered model-visible conversion or resize notices.
    pub hints: Vec<String>,
}

/// Send image conversion seam used by the read reference operation.
pub trait ReadImageProcessor: Send + Sync + 'static {
    /// Converts or resizes a detected supported image.
    fn process(
        &self,
        bytes: Bytes,
        mime_type: &str,
        auto_resize_images: bool,
    ) -> SendBoxFuture<'_, Result<ProcessedReadImage, String>>;
}

/// Local counterpart of [`ReadImageProcessor`].
pub trait LocalReadImageProcessor: 'static {
    /// Converts or resizes a detected supported image.
    fn process(
        &self,
        bytes: Bytes,
        mime_type: &str,
        auto_resize_images: bool,
    ) -> LocalBoxFuture<'_, Result<ProcessedReadImage, String>>;
}

/// Read-tool configuration shared by the Send and Local operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadToolOptions {
    /// Whether an injected image processor should resize images.
    pub auto_resize_images: bool,
    /// Maximum model-visible UTF-8 bytes.
    pub max_bytes: usize,
    /// Maximum model-visible logical lines.
    pub max_lines: usize,
}

impl Default for ReadToolOptions {
    fn default() -> Self {
        Self {
            auto_resize_images: true,
            max_bytes: DEFAULT_TOOL_MAX_BYTES,
            max_lines: DEFAULT_TOOL_MAX_LINES,
        }
    }
}

/// Per-canonical-path asynchronous mutation serializer.
#[derive(Debug, Default)]
pub struct FileMutationQueue {
    locks: DashMap<CanonicalPath, Arc<AsyncMutex<()>>>,
}

impl FileMutationQueue {
    /// Creates an empty mutation queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `operation` after all earlier operations for `path` have settled.
    ///
    /// Operations for different canonical paths remain independently pollable.
    pub async fn with_path_lock<T>(
        &self,
        path: CanonicalPath,
        operation: impl Future<Output = T>,
    ) -> T {
        let path_lock = self
            .locks
            .entry(path.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        let guard = path_lock.lock().await;
        let result = operation.await;
        drop(guard);

        if let Entry::Occupied(entry) = self.locks.entry(path)
            && Arc::ptr_eq(entry.get(), &path_lock)
            && Arc::strong_count(&path_lock) == 2
        {
            entry.remove();
        }
        result
    }

    /// Returns the number of path locks retained for active or queued work.
    pub fn active_paths(&self) -> usize {
        self.locks.len()
    }
}

/// Reads text or a supported image through the Send filesystem capability.
///
/// Text uses Pi's one-based offset, caller limit, 2,000-line/50-KiB default
/// truncation, and continuation notices. Image type is detected from bytes,
/// not the file extension.
pub async fn read_tool(
    filesystem: &dyn AgentFileSystem,
    path: &AgentPath,
    request: ReadToolRequest,
    options: ReadToolOptions,
    image_processor: Option<&dyn ReadImageProcessor>,
    cancellation: CancellationToken,
) -> Result<ToolOutput, FileSystemError> {
    let read = filesystem
        .read(path, ReadLimits::default(), cancellation)
        .await?;
    read_tool_output(path, read.data, request, options, image_processor).await
}

/// Local counterpart of [`read_tool`].
pub async fn read_local_tool(
    filesystem: &dyn LocalAgentFileSystem,
    path: &AgentPath,
    request: ReadToolRequest,
    options: ReadToolOptions,
    image_processor: Option<&dyn LocalReadImageProcessor>,
    cancellation: CancellationToken,
) -> Result<ToolOutput, FileSystemError> {
    let read = filesystem
        .read(path, ReadLimits::default(), cancellation)
        .await?;
    read_local_tool_output(path, read.data, request, options, image_processor).await
}

async fn read_tool_output(
    path: &AgentPath,
    data: Bytes,
    request: ReadToolRequest,
    options: ReadToolOptions,
    image_processor: Option<&dyn ReadImageProcessor>,
) -> Result<ToolOutput, FileSystemError> {
    if let Some(mime_type) = detect_supported_image_mime_type(&data) {
        return if let Some(processor) = image_processor {
            match processor
                .process(data, mime_type, options.auto_resize_images)
                .await
            {
                Ok(image) => Ok(processed_image_output(image)),
                Err(message) => Ok(image_failure_output(mime_type, &message)),
            }
        } else {
            Ok(unprocessed_image_output(data, mime_type))
        };
    }
    text_read_output(path, &data, request, options)
}

async fn read_local_tool_output(
    path: &AgentPath,
    data: Bytes,
    request: ReadToolRequest,
    options: ReadToolOptions,
    image_processor: Option<&dyn LocalReadImageProcessor>,
) -> Result<ToolOutput, FileSystemError> {
    if let Some(mime_type) = detect_supported_image_mime_type(&data) {
        return if let Some(processor) = image_processor {
            match processor
                .process(data, mime_type, options.auto_resize_images)
                .await
            {
                Ok(image) => Ok(processed_image_output(image)),
                Err(message) => Ok(image_failure_output(mime_type, &message)),
            }
        } else {
            Ok(unprocessed_image_output(data, mime_type))
        };
    }
    text_read_output(path, &data, request, options)
}

fn text_read_output(
    path: &AgentPath,
    data: &[u8],
    request: ReadToolRequest,
    options: ReadToolOptions,
) -> Result<ToolOutput, FileSystemError> {
    let text = String::from_utf8_lossy(data);
    let lines = text.split('\n').collect::<Vec<_>>();
    let start = request.offset.unwrap_or(1).saturating_sub(1);
    if start >= lines.len() {
        return Err(FileSystemError::Invalid {
            path: path.clone(),
            message: format!(
                "Offset {} is beyond end of file ({} lines total)",
                request.offset.unwrap_or(1),
                lines.len()
            ),
        });
    }

    let end = request.limit.map_or(lines.len(), |limit| {
        start.saturating_add(limit).min(lines.len())
    });
    let selected = lines[start..end].join("\n");
    let truncation = truncate(
        &selected,
        TruncationLimits {
            max_bytes: options.max_bytes,
            max_lines: options.max_lines,
            strategy: TruncationStrategy::Head,
        },
    );
    let start_display = start + 1;
    let mut output = truncation.content.clone();
    let details = if truncation.first_line_exceeds_limit {
        let first_line_bytes = lines[start].len();
        output = format!(
            "[Line {start_display} is {}, exceeds {} limit. Use bash: sed -n '{start_display}p' {path} | head -c {}]",
            format_size(first_line_bytes as u64),
            format_size(options.max_bytes as u64),
            options.max_bytes
        );
        Some(ReadToolDetails {
            schema_version: READ_TOOL_DETAILS_SCHEMA_VERSION,
            truncation,
        })
    } else if truncation.truncated {
        let end_display = start_display + truncation.output_lines.saturating_sub(1) as usize;
        let qualifier = if truncation.truncated_by == Some(crate::TruncatedBy::Bytes) {
            format!(" ({} limit)", format_size(options.max_bytes as u64))
        } else {
            String::new()
        };
        output.push_str(&format!(
            "\n\n[Showing lines {start_display}-{end_display} of {}{qualifier}. Use offset={} to continue.]",
            logical_file_line_count(&text),
            end_display + 1
        ));
        Some(ReadToolDetails {
            schema_version: READ_TOOL_DETAILS_SCHEMA_VERSION,
            truncation,
        })
    } else if request.limit.is_some() && end < lines.len() {
        output.push_str(&format!(
            "\n\n[{} more lines in file. Use offset={} to continue.]",
            lines.len() - end,
            end + 1
        ));
        None
    } else {
        None
    };

    let mut result = ToolOutput::new(vec![ToolResultContent::Text {
        id: ContentBlockId::new("read-text"),
        text: output,
    }]);
    result.details = details
        .map(|details| serde_json::value::to_raw_value(&details))
        .transpose()
        .map_err(|error| FileSystemError::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
    Ok(result)
}

fn logical_file_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else if text.ends_with('\n') {
        text.split('\n').count().saturating_sub(1)
    } else {
        text.split('\n').count()
    }
}

fn detect_supported_image_mime_type(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

fn processed_image_output(image: ProcessedReadImage) -> ToolOutput {
    let hints = if image.hints.is_empty() {
        String::new()
    } else {
        format!("\n{}", image.hints.join("\n"))
    };
    ToolOutput::new(vec![
        ToolResultContent::Text {
            id: ContentBlockId::new("read-image-notice"),
            text: format!("Read image file [{}]{hints}", image.mime_type),
        },
        ToolResultContent::Image {
            id: ContentBlockId::new("read-image"),
            data: image.data,
            mime_type: image.mime_type,
        },
    ])
}

fn image_failure_output(mime_type: &str, message: &str) -> ToolOutput {
    ToolOutput::new(vec![ToolResultContent::Text {
        id: ContentBlockId::new("read-image-notice"),
        text: format!("Read image file [{mime_type}]\n{message}"),
    }])
}

fn unprocessed_image_output(data: Bytes, mime_type: &str) -> ToolOutput {
    if mime_type == "image/bmp" {
        return image_failure_output(
            mime_type,
            "[Image omitted: configure an imageProcessor to convert BMP images.]",
        );
    }
    ToolOutput::new(vec![
        ToolResultContent::Text {
            id: ContentBlockId::new("read-image-notice"),
            text: format!("Read image file [{mime_type}]"),
        },
        ToolResultContent::Image {
            id: ContentBlockId::new("read-image"),
            data: BASE64_STANDARD.encode(data),
            mime_type: mime_type.to_owned(),
        },
    ])
}

/// Creates or overwrites a file while holding the canonical-path mutation lock.
/// Cancellation is checked both before and after the write, so the queue is not
/// released while an aborted host write is still settling.
pub async fn write_tool(
    filesystem: &dyn AgentFileSystem,
    mutations: &FileMutationQueue,
    path: &AgentPath,
    content: &str,
    cancellation: CancellationToken,
) -> Result<ToolOutput, FileSystemError> {
    let canonical = mutation_key(filesystem, path).await?;
    mutations
        .with_path_lock(canonical, async move {
            cancellation
                .check()
                .map_err(|_| FileSystemError::Cancelled { path: path.clone() })?;
            let write = filesystem
                .write(
                    path,
                    Bytes::copy_from_slice(content.as_bytes()),
                    cancellation.clone(),
                )
                .await?;
            cancellation
                .check()
                .map_err(|_| FileSystemError::Cancelled { path: path.clone() })?;
            Ok(ToolOutput::new(vec![ToolResultContent::Text {
                id: ContentBlockId::new("write-text"),
                text: format!("Successfully wrote {} bytes to {path}", write.bytes_written),
            }]))
        })
        .await
}

/// Local counterpart of [`write_tool`].
pub async fn write_local_tool(
    filesystem: &dyn LocalAgentFileSystem,
    mutations: &FileMutationQueue,
    path: &AgentPath,
    content: &str,
    cancellation: CancellationToken,
) -> Result<ToolOutput, FileSystemError> {
    let canonical = mutation_local_key(filesystem, path).await?;
    mutations
        .with_path_lock(canonical, async move {
            cancellation
                .check()
                .map_err(|_| FileSystemError::Cancelled { path: path.clone() })?;
            let write = filesystem
                .write(
                    path,
                    Bytes::copy_from_slice(content.as_bytes()),
                    cancellation.clone(),
                )
                .await?;
            cancellation
                .check()
                .map_err(|_| FileSystemError::Cancelled { path: path.clone() })?;
            Ok(ToolOutput::new(vec![ToolResultContent::Text {
                id: ContentBlockId::new("write-text"),
                text: format!("Successfully wrote {} bytes to {path}", write.bytes_written),
            }]))
        })
        .await
}

async fn mutation_key(
    filesystem: &dyn AgentFileSystem,
    path: &AgentPath,
) -> Result<CanonicalPath, FileSystemError> {
    match filesystem.canonicalize(path).await {
        Ok(path) => Ok(path),
        Err(FileSystemError::NotFound { .. }) => {
            Ok(CanonicalPath::new(path.as_path().to_path_buf()))
        }
        Err(error) => Err(error),
    }
}

async fn mutation_local_key(
    filesystem: &dyn LocalAgentFileSystem,
    path: &AgentPath,
) -> Result<CanonicalPath, FileSystemError> {
    match filesystem.canonicalize(path).await {
        Ok(path) => Ok(path),
        Err(FileSystemError::NotFound { .. }) => {
            Ok(CanonicalPath::new(path.as_path().to_path_buf()))
        }
        Err(error) => Err(error),
    }
}

/// One replacement requested from the reference edit tool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditReplacement {
    /// Text that must identify one unique original-file region after Pi's
    /// exact-then-fuzzy matching normalization.
    pub old_text: String,
    /// Replacement text for that region.
    pub new_text: String,
}

impl EditReplacement {
    /// Creates one replacement.
    pub fn new(old_text: impl Into<String>, new_text: impl Into<String>) -> Self {
        Self {
            old_text: old_text.into(),
            new_text: new_text.into(),
        }
    }
}

/// One structured edit hunk.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditDiffHunk {
    /// One-based old-file start line.
    pub old_start_line: u64,
    /// Logical lines removed by the replacement.
    pub old_line_count: u64,
    /// One-based new-file start line.
    pub new_start_line: u64,
    /// Logical lines inserted by the replacement.
    pub new_line_count: u64,
    /// Exact removed text.
    pub removed: String,
    /// Exact inserted text.
    pub added: String,
}

/// Structured diff for one edit operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditDiff {
    /// Canonical edited path.
    pub path: String,
    /// Exact replacement hunks in source order.
    pub hunks: Vec<EditDiffHunk>,
}

/// Details returned by the reference edit operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditToolResultDetails {
    /// Structured textual change.
    pub diff: EditDiff,
    /// Filesystem metadata from the atomic replacement.
    pub metadata: EditResult,
}

/// Applies one or more Pi-compatible replacements under a canonical path lock.
///
/// Every replacement is matched against the same original content. Exact
/// matching is attempted first; when necessary, Pi's trailing-whitespace and
/// Unicode normalization is used. Ambiguous and overlapping regions are
/// rejected before the filesystem is mutated. BOM and the original line-ending
/// style are retained.
pub async fn edit_file(
    filesystem: &dyn AgentFileSystem,
    mutations: &FileMutationQueue,
    path: &AgentPath,
    edits: &[EditReplacement],
    cancellation: CancellationToken,
) -> Result<EditToolResultDetails, FileSystemError> {
    let canonical = filesystem.canonicalize(path).await?;
    let locked_path = AgentPath::new(canonical.as_path().to_path_buf());
    let edits = edits.to_vec();
    mutations
        .with_path_lock(canonical.clone(), async move {
            let read = filesystem
                .read(&locked_path, ReadLimits::default(), cancellation.clone())
                .await?;
            let content = String::from_utf8(read.data.to_vec()).map_err(|error| {
                FileSystemError::Invalid {
                    path: locked_path.clone(),
                    message: format!("file is not valid UTF-8: {error}"),
                }
            })?;
            let prepared = prepare_edits(&canonical, &locked_path, &content, &edits)?;
            let metadata = filesystem
                .replace_exact(
                    &locked_path,
                    &content,
                    &prepared.final_content,
                    cancellation.clone(),
                )
                .await?;
            cancellation
                .check()
                .map_err(|_| FileSystemError::Cancelled {
                    path: locked_path.clone(),
                })?;
            Ok(EditToolResultDetails {
                diff: prepared.diff,
                metadata,
            })
        })
        .await
}

/// Applies Pi-compatible replacements through a local filesystem capability.
pub async fn edit_local_file(
    filesystem: &dyn LocalAgentFileSystem,
    mutations: &FileMutationQueue,
    path: &AgentPath,
    edits: &[EditReplacement],
    cancellation: CancellationToken,
) -> Result<EditToolResultDetails, FileSystemError> {
    let canonical = filesystem.canonicalize(path).await?;
    let locked_path = AgentPath::new(canonical.as_path().to_path_buf());
    let edits = edits.to_vec();
    mutations
        .with_path_lock(canonical.clone(), async move {
            let read = filesystem
                .read(&locked_path, ReadLimits::default(), cancellation.clone())
                .await?;
            let content = String::from_utf8(read.data.to_vec()).map_err(|error| {
                FileSystemError::Invalid {
                    path: locked_path.clone(),
                    message: format!("file is not valid UTF-8: {error}"),
                }
            })?;
            let prepared = prepare_edits(&canonical, &locked_path, &content, &edits)?;
            let metadata = filesystem
                .replace_exact(
                    &locked_path,
                    &content,
                    &prepared.final_content,
                    cancellation.clone(),
                )
                .await?;
            cancellation
                .check()
                .map_err(|_| FileSystemError::Cancelled {
                    path: locked_path.clone(),
                })?;
            Ok(EditToolResultDetails {
                diff: prepared.diff,
                metadata,
            })
        })
        .await
}

/// Convenience wrapper for one replacement through the Send filesystem seam.
pub async fn edit_file_exact(
    filesystem: &dyn AgentFileSystem,
    mutations: &FileMutationQueue,
    path: &AgentPath,
    expected: &str,
    replacement: &str,
    cancellation: CancellationToken,
) -> Result<EditToolResultDetails, FileSystemError> {
    edit_file(
        filesystem,
        mutations,
        path,
        &[EditReplacement::new(expected, replacement)],
        cancellation,
    )
    .await
}

/// Convenience wrapper for one replacement through the local filesystem seam.
pub async fn edit_local_file_exact(
    filesystem: &dyn LocalAgentFileSystem,
    mutations: &FileMutationQueue,
    path: &AgentPath,
    expected: &str,
    replacement: &str,
    cancellation: CancellationToken,
) -> Result<EditToolResultDetails, FileSystemError> {
    edit_local_file(
        filesystem,
        mutations,
        path,
        &[EditReplacement::new(expected, replacement)],
        cancellation,
    )
    .await
}

/// Persistable metadata for a bounded bash result and its complete-output artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BashToolResultDetails {
    /// Native schema version.
    pub schema_version: u32,
    /// Process exit code, absent when termination was signal-only.
    pub exit_code: Option<i32>,
    /// Portable signal label, when reported.
    pub signal: Option<String>,
    /// Whether the displayed output omits complete output.
    pub truncated: bool,
    /// UTF-8 bytes in complete output.
    pub total_bytes: u64,
    /// Logical lines in complete output.
    pub total_lines: u64,
    /// Host-owned complete-output artifact, created only when truncated.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_artifact_ref"
    )]
    pub full_output_artifact: Option<ArtifactRef>,
}

/// Display output and structured recovery details for one bash invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BashToolResult {
    /// Bounded model-visible output.
    pub output: String,
    /// Process, accounting, and recovery metadata.
    pub details: BashToolResultDetails,
    /// Complete truncation accounting.
    pub truncation: TruncationResult,
}

/// Mutable, executor-neutral shell execution passed through Pi's optional
/// asynchronous `prepare` hook before process creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BashExecutionRequest {
    /// Exact shell program or path.
    pub shell: String,
    /// Command submitted to the shell.
    pub command: String,
    /// Optional command prepended on its own line.
    pub command_prefix: Option<String>,
    /// Optional working directory.
    pub current_dir: Option<AgentPath>,
    /// Explicit environment overlay.
    pub environment: BTreeMap<String, String>,
    /// Whether the process inherits the host environment.
    pub inherit_environment: bool,
    /// Optional positive execution timeout.
    pub timeout: Option<Duration>,
    /// Model-visible output limits.
    pub limits: TruncationLimits,
}

impl BashExecutionRequest {
    /// Creates a request using `bash -lc`, inherited environment, no timeout,
    /// and Pi's default output limits.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            shell: "bash".into(),
            command: command.into(),
            command_prefix: None,
            current_dir: None,
            environment: BTreeMap::new(),
            inherit_environment: true,
            timeout: None,
            limits: TruncationLimits {
                max_bytes: DEFAULT_TOOL_MAX_BYTES,
                max_lines: DEFAULT_TOOL_MAX_LINES,
                strategy: TruncationStrategy::Tail,
            },
        }
    }

    /// Produces the portable process command after applying the prefix.
    pub fn process_command(&self) -> ProcessCommand {
        let command = self.command_prefix.as_ref().map_or_else(
            || self.command.clone(),
            |prefix| format!("{prefix}\n{}", self.command),
        );
        let mut process =
            ProcessCommand::new(self.shell.clone()).with_arguments(["-lc".to_owned(), command]);
        process.current_dir.clone_from(&self.current_dir);
        process.environment.clone_from(&self.environment);
        process.inherit_environment = self.inherit_environment;
        process
    }
}

/// Sanitized failure returned by an application-provided Bash prepare hook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BashPrepareError {
    /// Host-visible failure detail.
    pub message: String,
}

impl BashPrepareError {
    /// Creates a prepare-hook failure.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for BashPrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BashPrepareError {}

/// Send-capable asynchronous Bash prepare hook.
///
/// The hook receives the caller's exact turn context and logical cancellation
/// signal. It may mutate the command, working directory, environment, and
/// inheritance policy before the process is spawned.
pub trait BashPrepare<C: ?Sized>: Send + Sync + 'static {
    /// Mutates one execution after command-prefix application and before spawn.
    fn prepare<'a>(
        &'a self,
        execution: &'a mut BashExecutionRequest,
        context: &'a C,
        cancellation: &'a CancellationToken,
    ) -> SendBoxFuture<'a, Result<(), BashPrepareError>>;
}

/// Local-executor counterpart of [`BashPrepare`].
pub trait LocalBashPrepare<C: ?Sized>: 'static {
    /// Mutates one execution after command-prefix application and before spawn.
    fn prepare<'a>(
        &'a self,
        execution: &'a mut BashExecutionRequest,
        context: &'a C,
        cancellation: &'a CancellationToken,
    ) -> LocalBoxFuture<'a, Result<(), BashPrepareError>>;
}

/// Successful shell result after process joining and output recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BashExecutionResult {
    /// Bounded model-visible result and complete-output reference.
    pub result: BashToolResult,
    /// Portable process termination outcome.
    pub outcome: ProcessOutcome,
}

/// Send callback for coalesced, bounded bash output updates.
pub trait BashUpdateSink: Send + Sync + 'static {
    /// Receives one current output snapshot. No callback occurs after the
    /// execution future settles.
    fn update(&self, update: BashToolResult);
}

/// Local counterpart of [`BashUpdateSink`].
pub trait LocalBashUpdateSink: 'static {
    /// Receives one current output snapshot. No callback occurs after the
    /// execution future settles.
    fn update(&self, update: BashToolResult);
}

/// Failure from executor-neutral shell execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BashExecutionError {
    /// The application-provided prepare hook rejected the execution.
    Prepare(BashPrepareError),
    /// Process creation, observation, or termination failed.
    Process(ProcessError),
    /// Complete output could not be persisted.
    Artifact(ArtifactError),
    /// Timeout observation failed through the host clock capability.
    Clock(ClockError),
    /// The process exited unsuccessfully after producing this recoverable result.
    NonZero {
        /// Complete bounded and persisted output result.
        result: Box<BashExecutionResult>,
    },
    /// The configured deadline terminated the process after producing this result.
    TimedOut {
        /// Requested timeout.
        timeout: Duration,
        /// Complete bounded and persisted output result.
        result: Box<BashExecutionResult>,
    },
    /// Caller cancellation terminated the process after producing this result.
    Cancelled {
        /// Complete bounded and persisted output result.
        result: Box<BashExecutionResult>,
    },
}

impl fmt::Display for BashExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare(error) => error.fmt(formatter),
            Self::Process(error) => error.fmt(formatter),
            Self::Artifact(error) => error.fmt(formatter),
            Self::Clock(error) => error.fmt(formatter),
            Self::NonZero { result } => formatter.write_str(&append_bash_status(
                &result.result.output,
                result.outcome.status.code.map_or_else(
                    || "Command failed".into(),
                    |code| format!("Command exited with code {code}"),
                ),
            )),
            Self::TimedOut { timeout, result } => formatter.write_str(&append_bash_status(
                &result.result.output,
                format!("Command timed out after {} seconds", timeout.as_secs_f64()),
            )),
            Self::Cancelled { result } => formatter.write_str(&append_bash_status(
                &result.result.output,
                "Command aborted".into(),
            )),
        }
    }
}

impl std::error::Error for BashExecutionError {}

impl From<BashPrepareError> for BashExecutionError {
    fn from(error: BashPrepareError) -> Self {
        Self::Prepare(error)
    }
}

impl From<ProcessError> for BashExecutionError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<ArtifactError> for BashExecutionError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<ClockError> for BashExecutionError {
    fn from(error: ClockError) -> Self {
        Self::Clock(error)
    }
}

fn execution_after_prefix(request: &BashExecutionRequest) -> BashExecutionRequest {
    let mut execution = request.clone();
    if let Some(prefix) = execution.command_prefix.take() {
        execution.command = format!("{prefix}\n{}", execution.command);
    }
    execution
}

/// Applies an asynchronous Send prepare hook, then executes the mutated Bash
/// request. Prefix application precedes the hook exactly as in pinned Pi.
pub async fn execute_bash_tool_with_prepare<C: ?Sized + 'static>(
    processes: &dyn ProcessSpawner,
    clock: &dyn Clock,
    artifacts: &dyn TemporaryArtifactStore,
    request: &BashExecutionRequest,
    context: &C,
    prepare: &dyn BashPrepare<C>,
    cancellation: CancellationToken,
) -> Result<BashExecutionResult, BashExecutionError> {
    execute_bash_tool_with_prepare_and_updates(
        processes,
        clock,
        artifacts,
        request,
        context,
        prepare,
        None,
        cancellation,
    )
    .await
}

/// Applies an asynchronous Send prepare hook and executes with output updates.
#[allow(
    clippy::too_many_arguments,
    reason = "the reference tool keeps each injected capability and Pi hook input explicit"
)]
pub async fn execute_bash_tool_with_prepare_and_updates<C: ?Sized + 'static>(
    processes: &dyn ProcessSpawner,
    clock: &dyn Clock,
    artifacts: &dyn TemporaryArtifactStore,
    request: &BashExecutionRequest,
    context: &C,
    prepare: &dyn BashPrepare<C>,
    updates: Option<&dyn BashUpdateSink>,
    cancellation: CancellationToken,
) -> Result<BashExecutionResult, BashExecutionError> {
    let mut execution = execution_after_prefix(request);
    prepare
        .prepare(&mut execution, context, &cancellation)
        .await?;
    execute_bash_tool_with_updates(
        processes,
        clock,
        artifacts,
        &execution,
        updates,
        cancellation,
    )
    .await
}

/// Applies an asynchronous Local prepare hook, then executes the mutated Bash
/// request. Prefix application precedes the hook exactly as in pinned Pi.
pub async fn execute_local_bash_tool_with_prepare<C: ?Sized + 'static>(
    processes: &dyn LocalProcessSpawner,
    clock: &dyn LocalClock,
    artifacts: &dyn LocalTemporaryArtifactStore,
    request: &BashExecutionRequest,
    context: &C,
    prepare: &dyn LocalBashPrepare<C>,
    cancellation: CancellationToken,
) -> Result<BashExecutionResult, BashExecutionError> {
    execute_local_bash_tool_with_prepare_and_updates(
        processes,
        clock,
        artifacts,
        request,
        context,
        prepare,
        None,
        cancellation,
    )
    .await
}

/// Applies a Local prepare hook and executes with local output updates.
#[allow(
    clippy::too_many_arguments,
    reason = "the Local reference tool mirrors the explicit Send capability boundary"
)]
pub async fn execute_local_bash_tool_with_prepare_and_updates<C: ?Sized + 'static>(
    processes: &dyn LocalProcessSpawner,
    clock: &dyn LocalClock,
    artifacts: &dyn LocalTemporaryArtifactStore,
    request: &BashExecutionRequest,
    context: &C,
    prepare: &dyn LocalBashPrepare<C>,
    updates: Option<&dyn LocalBashUpdateSink>,
    cancellation: CancellationToken,
) -> Result<BashExecutionResult, BashExecutionError> {
    let mut execution = execution_after_prefix(request);
    prepare
        .prepare(&mut execution, context, &cancellation)
        .await?;
    execute_local_bash_tool_with_updates(
        processes,
        clock,
        artifacts,
        &execution,
        updates,
        cancellation,
    )
    .await
}

/// Executes a prepared shell request through Send environment capabilities.
///
/// Stdout and stderr are combined in event order. Every timeout or caller
/// cancellation joins the process before returning, and its output is passed
/// through the same truncation and artifact path as a successful command.
pub async fn execute_bash_tool(
    processes: &dyn ProcessSpawner,
    clock: &dyn Clock,
    artifacts: &dyn TemporaryArtifactStore,
    request: &BashExecutionRequest,
    cancellation: CancellationToken,
) -> Result<BashExecutionResult, BashExecutionError> {
    execute_bash_tool_with_updates(processes, clock, artifacts, request, None, cancellation).await
}

/// Executes a prepared shell request and emits bounded, coalesced snapshots.
pub async fn execute_bash_tool_with_updates(
    processes: &dyn ProcessSpawner,
    clock: &dyn Clock,
    artifacts: &dyn TemporaryArtifactStore,
    request: &BashExecutionRequest,
    updates: Option<&dyn BashUpdateSink>,
    cancellation: CancellationToken,
) -> Result<BashExecutionResult, BashExecutionError> {
    cancellation.check().map_err(|_| ProcessError::Cancelled)?;
    let process_cancellation = cancellation.child();
    let mut process = processes
        .spawn(request.process_command(), process_cancellation.clone())
        .await?;
    let output = Arc::new(SyncMutex::new(Vec::new()));
    let (outcome, timed_out) = collect_send_process(
        process.as_mut(),
        clock,
        ProcessCollectionOptions {
            timeout: request.timeout,
            limits: request.limits,
        },
        Arc::clone(&output),
        updates,
        process_cancellation,
        &cancellation,
    )
    .await?;
    let complete = String::from_utf8_lossy(&lock_sync(&output)).into_owned();
    let result = prepare_bash_tool_result(
        artifacts,
        &complete,
        &outcome.status,
        request.limits,
        CancellationToken::new(),
    )
    .await?;
    let execution = BashExecutionResult {
        result: with_bash_display_notice(result),
        outcome,
    };
    if let Some(updates) = updates {
        updates.update(execution.result.clone());
    }
    classify_bash_execution(
        execution,
        request.timeout,
        timed_out,
        cancellation.is_cancelled(),
    )
}

/// Local counterpart of [`execute_bash_tool`].
pub async fn execute_local_bash_tool(
    processes: &dyn LocalProcessSpawner,
    clock: &dyn LocalClock,
    artifacts: &dyn LocalTemporaryArtifactStore,
    request: &BashExecutionRequest,
    cancellation: CancellationToken,
) -> Result<BashExecutionResult, BashExecutionError> {
    execute_local_bash_tool_with_updates(processes, clock, artifacts, request, None, cancellation)
        .await
}

/// Local counterpart of [`execute_bash_tool_with_updates`].
pub async fn execute_local_bash_tool_with_updates(
    processes: &dyn LocalProcessSpawner,
    clock: &dyn LocalClock,
    artifacts: &dyn LocalTemporaryArtifactStore,
    request: &BashExecutionRequest,
    updates: Option<&dyn LocalBashUpdateSink>,
    cancellation: CancellationToken,
) -> Result<BashExecutionResult, BashExecutionError> {
    cancellation.check().map_err(|_| ProcessError::Cancelled)?;
    let process_cancellation = cancellation.child();
    let mut process = processes
        .spawn(request.process_command(), process_cancellation.clone())
        .await?;
    let output = Arc::new(SyncMutex::new(Vec::new()));
    let (outcome, timed_out) = collect_local_process(
        process.as_mut(),
        clock,
        ProcessCollectionOptions {
            timeout: request.timeout,
            limits: request.limits,
        },
        Arc::clone(&output),
        updates,
        process_cancellation,
        &cancellation,
    )
    .await?;
    let complete = String::from_utf8_lossy(&lock_sync(&output)).into_owned();
    let result = prepare_local_bash_tool_result(
        artifacts,
        &complete,
        &outcome.status,
        request.limits,
        CancellationToken::new(),
    )
    .await?;
    let execution = BashExecutionResult {
        result: with_bash_display_notice(result),
        outcome,
    };
    if let Some(updates) = updates {
        updates.update(execution.result.clone());
    }
    classify_bash_execution(
        execution,
        request.timeout,
        timed_out,
        cancellation.is_cancelled(),
    )
}

#[derive(Clone, Copy)]
struct ProcessCollectionOptions {
    timeout: Option<Duration>,
    limits: TruncationLimits,
}

async fn collect_send_process(
    process: &mut dyn RunningProcess,
    clock: &dyn Clock,
    options: ProcessCollectionOptions,
    output: Arc<SyncMutex<Vec<u8>>>,
    updates: Option<&dyn BashUpdateSink>,
    process_cancellation: CancellationToken,
    cancellation: &CancellationToken,
) -> Result<(ProcessOutcome, bool), BashExecutionError> {
    let Some(timeout) = options.timeout else {
        return collect_send_events(process, output, options.limits, updates)
            .await
            .map(|outcome| (outcome, false))
            .map_err(Into::into);
    };
    if timeout.is_zero() {
        return Err(ProcessError::Spawn {
            message: "timeout must be positive".into(),
        }
        .into());
    }

    let collection = Box::pin(collect_send_events(
        process,
        output,
        options.limits,
        updates,
    ));
    let deadline = Box::pin(clock.sleep(timeout, cancellation.child()));
    let race = match select(collection, deadline).await {
        Either::Left((outcome, _deadline)) => ProcessRace::Completed(outcome),
        Either::Right((deadline, collection)) => {
            drop(collection);
            ProcessRace::Deadline(deadline)
        }
    };
    match race {
        ProcessRace::Completed(outcome) => Ok((outcome?, false)),
        ProcessRace::Deadline(deadline) => {
            process_cancellation.cancel();
            let outcome = process.terminate(TerminationPolicy::default()).await?;
            match deadline {
                Ok(()) => Ok((outcome, !cancellation.is_cancelled())),
                Err(ClockError::Cancelled) if cancellation.is_cancelled() => Ok((outcome, false)),
                Err(error) => Err(error.into()),
            }
        }
    }
}

async fn collect_local_process(
    process: &mut dyn LocalRunningProcess,
    clock: &dyn LocalClock,
    options: ProcessCollectionOptions,
    output: Arc<SyncMutex<Vec<u8>>>,
    updates: Option<&dyn LocalBashUpdateSink>,
    process_cancellation: CancellationToken,
    cancellation: &CancellationToken,
) -> Result<(ProcessOutcome, bool), BashExecutionError> {
    let Some(timeout) = options.timeout else {
        return collect_local_events(process, output, options.limits, updates)
            .await
            .map(|outcome| (outcome, false))
            .map_err(Into::into);
    };
    if timeout.is_zero() {
        return Err(ProcessError::Spawn {
            message: "timeout must be positive".into(),
        }
        .into());
    }

    let collection = Box::pin(collect_local_events(
        process,
        output,
        options.limits,
        updates,
    ));
    let deadline = Box::pin(clock.sleep(timeout, cancellation.child()));
    let race = match select(collection, deadline).await {
        Either::Left((outcome, _deadline)) => ProcessRace::Completed(outcome),
        Either::Right((deadline, collection)) => {
            drop(collection);
            ProcessRace::Deadline(deadline)
        }
    };
    match race {
        ProcessRace::Completed(outcome) => Ok((outcome?, false)),
        ProcessRace::Deadline(deadline) => {
            process_cancellation.cancel();
            let outcome = process.terminate(TerminationPolicy::default()).await?;
            match deadline {
                Ok(()) => Ok((outcome, !cancellation.is_cancelled())),
                Err(ClockError::Cancelled) if cancellation.is_cancelled() => Ok((outcome, false)),
                Err(error) => Err(error.into()),
            }
        }
    }
}

enum ProcessRace {
    Completed(Result<ProcessOutcome, ProcessError>),
    Deadline(Result<(), ClockError>),
}

async fn collect_send_events(
    process: &mut dyn RunningProcess,
    output: Arc<SyncMutex<Vec<u8>>>,
    limits: TruncationLimits,
    updates: Option<&dyn BashUpdateSink>,
) -> Result<ProcessOutcome, ProcessError> {
    let mut bytes_at_last_update = 0;
    let mut events = process.events();
    while let Some(event) = events.next().await {
        match event? {
            ProcessEvent::Stdout(bytes) | ProcessEvent::Stderr(bytes) => {
                lock_sync(&output).extend_from_slice(&bytes);
                let total = lock_sync(&output).len();
                if total.saturating_sub(bytes_at_last_update) >= 4 * 1_024 {
                    if let Some(updates) = updates {
                        updates.update(intermediate_bash_result(&output, limits));
                    }
                    bytes_at_last_update = total;
                }
            }
            ProcessEvent::Exited(outcome) => return Ok(outcome),
            _ => {}
        }
    }
    Err(ProcessError::Io {
        message: "process event stream ended without an exit outcome".into(),
    })
}

async fn collect_local_events(
    process: &mut dyn LocalRunningProcess,
    output: Arc<SyncMutex<Vec<u8>>>,
    limits: TruncationLimits,
    updates: Option<&dyn LocalBashUpdateSink>,
) -> Result<ProcessOutcome, ProcessError> {
    let mut bytes_at_last_update = 0;
    let mut events = process.events();
    while let Some(event) = events.next().await {
        match event? {
            ProcessEvent::Stdout(bytes) | ProcessEvent::Stderr(bytes) => {
                lock_sync(&output).extend_from_slice(&bytes);
                let total = lock_sync(&output).len();
                if total.saturating_sub(bytes_at_last_update) >= 4 * 1_024 {
                    if let Some(updates) = updates {
                        updates.update(intermediate_bash_result(&output, limits));
                    }
                    bytes_at_last_update = total;
                }
            }
            ProcessEvent::Exited(outcome) => return Ok(outcome),
            _ => {}
        }
    }
    Err(ProcessError::Io {
        message: "process event stream ended without an exit outcome".into(),
    })
}

fn intermediate_bash_result(
    output: &SyncMutex<Vec<u8>>,
    mut limits: TruncationLimits,
) -> BashToolResult {
    limits.strategy = TruncationStrategy::Tail;
    let output = String::from_utf8_lossy(&lock_sync(output)).into_owned();
    build_bash_result(
        &ProcessExitStatus {
            code: None,
            signal: None,
            success: true,
        },
        truncate(&output, limits),
        None,
    )
}

fn classify_bash_execution(
    mut execution: BashExecutionResult,
    timeout: Option<Duration>,
    timed_out: bool,
    cancelled: bool,
) -> Result<BashExecutionResult, BashExecutionError> {
    if cancelled || execution.outcome.termination == ProcessTermination::Cancelled {
        return Err(BashExecutionError::Cancelled {
            result: Box::new(execution),
        });
    }
    if timed_out {
        return Err(BashExecutionError::TimedOut {
            timeout: timeout.expect("a timeout race requires a configured duration"),
            result: Box::new(execution),
        });
    }
    if !execution.outcome.status.success {
        return Err(BashExecutionError::NonZero {
            result: Box::new(execution),
        });
    }
    if execution.result.output.is_empty() {
        execution.result.output = "(no output)".into();
    }
    Ok(execution)
}

fn with_bash_display_notice(mut result: BashToolResult) -> BashToolResult {
    if !result.truncation.truncated {
        return result;
    }
    let artifact = result
        .details
        .full_output_artifact
        .as_ref()
        .expect("truncated bash output always has a recovery artifact");
    let end_line = result.truncation.total_lines;
    let notice = if result.truncation.last_line_partial {
        format!(
            "[Showing last {} of line {end_line} (line is {}). Full output: {}]",
            format_size(result.truncation.output_bytes),
            format_size(result.truncation.total_bytes),
            artifact.path
        )
    } else {
        let start_line = end_line
            .saturating_sub(result.truncation.output_lines)
            .saturating_add(1);
        let qualifier = if result.truncation.truncated_by == Some(crate::TruncatedBy::Bytes) {
            format!(
                " ({} limit)",
                format_size(result.truncation.max_bytes as u64)
            )
        } else {
            String::new()
        };
        format!(
            "[Showing lines {start_line}-{end_line} of {}{qualifier}. Full output: {}]",
            result.truncation.total_lines, artifact.path
        )
    };
    result.output = append_bash_status(&result.output, notice);
    result
}

fn append_bash_status(output: &str, status: String) -> String {
    if output.is_empty() {
        status
    } else {
        format!("{output}\n\n{status}")
    }
}

fn lock_sync<T>(mutex: &SyncMutex<T>) -> SyncMutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bounds bash output from the tail and stores complete output when needed.
pub async fn prepare_bash_tool_result(
    artifacts: &dyn TemporaryArtifactStore,
    output: &str,
    status: &ProcessExitStatus,
    mut limits: TruncationLimits,
    cancellation: CancellationToken,
) -> Result<BashToolResult, ArtifactError> {
    limits.strategy = TruncationStrategy::Tail;
    let truncation = truncate(output, limits);
    let artifact = if truncation.truncated {
        Some(
            artifacts
                .create(
                    TemporaryArtifactRequest {
                        prefix: "bash-".into(),
                        suffix: ".log".into(),
                    },
                    Bytes::copy_from_slice(output.as_bytes()),
                    cancellation,
                )
                .await?,
        )
    } else {
        None
    };
    Ok(build_bash_result(status, truncation, artifact))
}

/// Local counterpart of [`prepare_bash_tool_result`].
pub async fn prepare_local_bash_tool_result(
    artifacts: &dyn LocalTemporaryArtifactStore,
    output: &str,
    status: &ProcessExitStatus,
    mut limits: TruncationLimits,
    cancellation: CancellationToken,
) -> Result<BashToolResult, ArtifactError> {
    limits.strategy = TruncationStrategy::Tail;
    let truncation = truncate(output, limits);
    let artifact = if truncation.truncated {
        Some(
            artifacts
                .create(
                    TemporaryArtifactRequest {
                        prefix: "bash-".into(),
                        suffix: ".log".into(),
                    },
                    Bytes::copy_from_slice(output.as_bytes()),
                    cancellation,
                )
                .await?,
        )
    } else {
        None
    };
    Ok(build_bash_result(status, truncation, artifact))
}

struct PreparedEdits {
    final_content: String,
    diff: EditDiff,
}

#[derive(Clone)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

#[derive(Clone, Copy)]
struct LineSpan {
    start: usize,
    end: usize,
}

struct ReplacementGroup {
    start_line: usize,
    end_line: usize,
    replacements: Vec<MatchedEdit>,
}

fn prepare_edits(
    canonical: &CanonicalPath,
    path: &AgentPath,
    content: &str,
    edits: &[EditReplacement],
) -> Result<PreparedEdits, FileSystemError> {
    if edits.is_empty() {
        return Err(FileSystemError::Invalid {
            path: path.clone(),
            message: "edits must contain at least one replacement".into(),
        });
    }

    let (bom, content_without_bom) = content
        .strip_prefix('\u{feff}')
        .map_or(("", content), |without| ("\u{feff}", without));
    let line_ending = detect_line_ending(content_without_bom);
    let normalized_content = normalize_to_lf(content_without_bom);
    let normalized_edits = edits
        .iter()
        .map(|edit| EditReplacement {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect::<Vec<_>>();

    for (index, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(FileSystemError::Invalid {
                path: path.clone(),
                message: if normalized_edits.len() == 1 {
                    format!("oldText must not be empty in {path}.")
                } else {
                    format!("edits[{index}].oldText must not be empty in {path}.")
                },
            });
        }
    }

    let use_fuzzy_base = normalized_edits.iter().any(|edit| {
        find_fuzzy_match(&normalized_content, &edit.old_text).is_some_and(|(_, _, fuzzy)| fuzzy)
    });
    let replacement_base = if use_fuzzy_base {
        normalize_for_fuzzy_match(&normalized_content)
    } else {
        normalized_content.clone()
    };

    let mut matched = Vec::with_capacity(normalized_edits.len());
    for (edit_index, edit) in normalized_edits.iter().enumerate() {
        let Some((match_index, match_length, _)) =
            find_fuzzy_match(&replacement_base, &edit.old_text)
        else {
            return Err(FileSystemError::ExactMatchNotFound { path: path.clone() });
        };
        let fuzzy_old_text = normalize_for_fuzzy_match(&edit.old_text);
        let occurrences = normalize_for_fuzzy_match(&replacement_base)
            .match_indices(&fuzzy_old_text)
            .count();
        if occurrences > 1 {
            return Err(FileSystemError::MultipleExactMatches {
                path: path.clone(),
                matches: occurrences as u64,
            });
        }
        matched.push(MatchedEdit {
            edit_index,
            match_index,
            match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched.sort_by_key(|edit| edit.match_index);
    for pair in matched.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(FileSystemError::Invalid {
                path: path.clone(),
                message: format!(
                    "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                    previous.edit_index, current.edit_index
                ),
            });
        }
    }

    let new_content = if use_fuzzy_base {
        apply_replacements_preserving_unchanged_lines(
            &normalized_content,
            &replacement_base,
            &matched,
            path,
        )?
    } else {
        apply_replacements(&replacement_base, &matched, 0)
    };
    if normalized_content == new_content {
        return Err(FileSystemError::NoOpReplacement { path: path.clone() });
    }

    let diff = build_edit_diff(
        canonical,
        &normalized_content,
        &new_content,
        &matched,
        use_fuzzy_base,
    );
    let restored = restore_line_endings(&new_content, line_ending);
    Ok(PreparedEdits {
        final_content: format!("{bom}{restored}"),
        diff,
    })
}

fn find_fuzzy_match(content: &str, old_text: &str) -> Option<(usize, usize, bool)> {
    if let Some(index) = content.find(old_text) {
        return Some((index, old_text.len(), false));
    }
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    fuzzy_content
        .find(&fuzzy_old_text)
        .map(|index| (index, fuzzy_old_text.len(), true))
}

fn normalize_for_fuzzy_match(text: &str) -> String {
    text.nfkc()
        .collect::<String>()
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(|character| match character {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn detect_line_ending(content: &str) -> &'static str {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

fn restore_line_endings(text: &str, line_ending: &str) -> String {
    if line_ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_owned()
    }
}

fn apply_replacements(content: &str, replacements: &[MatchedEdit], offset: usize) -> String {
    let mut result = content.to_owned();
    for replacement in replacements.iter().rev() {
        let start = replacement.match_index - offset;
        result.replace_range(
            start..start + replacement.match_length,
            &replacement.new_text,
        );
    }
    result
}

fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[MatchedEdit],
    path: &AgentPath,
) -> Result<String, FileSystemError> {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(FileSystemError::Invalid {
            path: path.clone(),
            message:
                "cannot preserve unchanged lines because normalized content changed line count"
                    .into(),
        });
    }

    let mut groups: Vec<ReplacementGroup> = Vec::new();
    for replacement in replacements {
        let (start_line, end_line) =
            replacement_line_range(&base_lines, replacement).ok_or_else(|| {
                FileSystemError::Invalid {
                    path: path.clone(),
                    message: "replacement range is outside the base content".into(),
                }
            })?;
        if let Some(current) = groups.last_mut()
            && start_line < current.end_line
        {
            current.end_line = current.end_line.max(end_line);
            current.replacements.push(replacement.clone());
        } else {
            groups.push(ReplacementGroup {
                start_line,
                end_line,
                replacements: vec![replacement.clone()],
            });
        }
    }

    let mut original_line_index = 0;
    let mut result = String::new();
    for group in groups {
        for line in &original_lines[original_line_index..group.start_line] {
            result.push_str(line);
        }
        let group_start_offset = base_lines[group.start_line].start;
        let group_end_offset = base_lines[group.end_line - 1].end;
        result.push_str(&apply_replacements(
            &base_content[group_start_offset..group_end_offset],
            &group.replacements,
            group_start_offset,
        ));
        original_line_index = group.end_line;
    }
    for line in &original_lines[original_line_index..] {
        result.push_str(line);
    }
    Ok(result)
}

fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, character) in content.char_indices() {
        if character == '\n' {
            lines.push(&content[start..=index]);
            start = index + 1;
        }
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

fn line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = LineSpan {
                start: offset,
                end: offset + line.len(),
            };
            offset = span.end;
            span
        })
        .collect()
}

fn replacement_line_range(lines: &[LineSpan], replacement: &MatchedEdit) -> Option<(usize, usize)> {
    let replacement_start = replacement.match_index;
    let replacement_end = replacement.match_index + replacement.match_length;
    let start_line = lines
        .iter()
        .position(|line| replacement_start >= line.start && replacement_start < line.end)?;
    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    (end_line < lines.len()).then_some((start_line, end_line + 1))
}

fn build_edit_diff(
    canonical: &CanonicalPath,
    old_content: &str,
    new_content: &str,
    replacements: &[MatchedEdit],
    used_fuzzy_match: bool,
) -> EditDiff {
    if used_fuzzy_match {
        return EditDiff {
            path: canonical.to_string(),
            hunks: vec![EditDiffHunk {
                old_start_line: 1,
                old_line_count: logical_line_count(old_content),
                new_start_line: 1,
                new_line_count: logical_line_count(new_content),
                removed: old_content.to_owned(),
                added: new_content.to_owned(),
            }],
        };
    }

    let mut line_delta = 0_i64;
    let mut hunks = Vec::with_capacity(replacements.len());
    for replacement in replacements {
        let removed = &old_content
            [replacement.match_index..replacement.match_index + replacement.match_length];
        let old_start_line = old_content[..replacement.match_index]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u64
            + 1;
        let new_start_line = (old_start_line as i64 + line_delta).max(1) as u64;
        hunks.push(EditDiffHunk {
            old_start_line,
            old_line_count: logical_line_count(removed),
            new_start_line,
            new_line_count: logical_line_count(&replacement.new_text),
            removed: removed.to_owned(),
            added: replacement.new_text.clone(),
        });
        line_delta += replacement
            .new_text
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as i64
            - removed.bytes().filter(|byte| *byte == b'\n').count() as i64;
    }
    EditDiff {
        path: canonical.to_string(),
        hunks,
    }
}

fn logical_line_count(content: &str) -> u64 {
    if content.is_empty() {
        0
    } else {
        content.split_terminator('\n').count().max(1) as u64
    }
}

fn build_bash_result(
    status: &ProcessExitStatus,
    truncation: TruncationResult,
    artifact: Option<ArtifactRef>,
) -> BashToolResult {
    BashToolResult {
        output: truncation.content.clone(),
        details: BashToolResultDetails {
            schema_version: BASH_TOOL_DETAILS_SCHEMA_VERSION,
            exit_code: status.code,
            signal: status.signal.clone(),
            truncated: truncation.truncated,
            total_bytes: truncation.total_bytes,
            total_lines: truncation.total_lines,
            full_output_artifact: artifact,
        },
        truncation,
    }
}

mod optional_artifact_ref {
    use agentprism_env::{ArtifactRef, CanonicalPath};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S>(
        artifact: &Option<ArtifactRef>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        artifact
            .as_ref()
            .map(|artifact| artifact.path.to_string())
            .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<ArtifactRef>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer).map(|path| {
            path.map(|path| ArtifactRef {
                path: CanonicalPath::new(path),
            })
        })
    }
}
