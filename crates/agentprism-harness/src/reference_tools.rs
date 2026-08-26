//! Executor-neutral reference-tool helpers from Architecture v2 part 2 §7.11.

use crate::{TruncationLimits, TruncationResult, TruncationStrategy, truncate};
use agentprism_ai::CancellationToken;
use agentprism_env::{
    AgentFileSystem, AgentPath, ArtifactError, ArtifactRef, CanonicalPath, EditResult,
    FileSystemError, LocalAgentFileSystem, LocalTemporaryArtifactStore, ProcessExitStatus,
    ReadLimits, TemporaryArtifactRequest, TemporaryArtifactStore,
};
use bytes::Bytes;
use dashmap::{DashMap, mapref::entry::Entry};
use futures_util::lock::Mutex as AsyncMutex;
use serde::{Deserialize, Serialize};
use std::{future::Future, sync::Arc};
use unicode_normalization::UnicodeNormalization;

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
                    cancellation,
                )
                .await?;
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
                    cancellation,
                )
                .await?;
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
