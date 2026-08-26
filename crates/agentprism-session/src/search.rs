//! Format-agnostic scanning search over session storage projections.

use crate::{
    LocalBoxFuture, LocalSessionRepository, LocalSessionStorage, SendBoxFuture, SessionEntry,
    SessionError, SessionErrorKind, SessionId, SessionRepository, SessionStorage,
};
use agentprism_ai::{CancellationToken, Timestamp};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, rc::Rc, sync::Arc};

/// Stable canonical entry kind accepted by scanning-search filters.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEntryKind {
    /// Durable agent message.
    Message,
    /// Model selection change.
    ModelChange,
    /// Reasoning-level change.
    ReasoningChange,
    /// Active-tool change.
    ActiveToolsChange,
    /// Compaction result.
    Compaction,
    /// Branch summary.
    BranchSummary,
    /// Application-defined entry.
    Custom,
}

impl SessionEntryKind {
    fn of(entry: &SessionEntry) -> Self {
        match entry {
            SessionEntry::Message { .. } => Self::Message,
            SessionEntry::ModelChange { .. } => Self::ModelChange,
            SessionEntry::ReasoningChange { .. } => Self::ReasoningChange,
            SessionEntry::ActiveToolsChange { .. } => Self::ActiveToolsChange,
            SessionEntry::Compaction { .. } => Self::Compaction,
            SessionEntry::BranchSummary { .. } => Self::BranchSummary,
            SessionEntry::Custom { .. } => Self::Custom,
        }
    }
}

/// Options for format-agnostic scanning search.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSearchOptions {
    /// Restrict hits to these canonical entry kinds. `None` accepts every kind.
    pub entry_kinds: Option<BTreeSet<SessionEntryKind>>,
    /// Maximum number of hits. Zero intentionally returns no hits.
    pub limit: Option<usize>,
}

/// One scanning-search hit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSearchHit {
    /// Owning session.
    pub session_id: SessionId,
    /// Matching immutable entry.
    pub entry_id: crate::EntryId,
    /// Entry timestamp.
    pub timestamp: Timestamp,
    /// Searchable serialized entry and optional label.
    pub snippet: String,
}

/// Scans an arbitrary ordered Send storage source.
///
/// The source order and each session's global entry sequence order are retained.
pub fn search_session_storages<'a>(
    storages: &'a [Arc<dyn SessionStorage>],
    text: &'a str,
    options: SessionSearchOptions,
    cancellation: CancellationToken,
) -> SendBoxFuture<'a, Result<Vec<SessionSearchHit>, SessionError>> {
    Box::pin(async move {
        let normalized = normalize_query(text, &options);
        let Some(normalized) = normalized else {
            return Ok(Vec::new());
        };
        let mut hits = Vec::new();
        let mut seen = BTreeSet::new();
        for storage in storages {
            check_cancelled(&cancellation)?;
            let metadata = storage.metadata().await?;
            if !seen.insert(metadata.session_id.clone()) {
                return Err(duplicate_session(&metadata.session_id));
            }
            let state = storage.load_state().await?;
            scan_state(
                &metadata.session_id,
                &state,
                &normalized,
                &options,
                &cancellation,
                &mut hits,
            )?;
            if reached_limit(&hits, options.limit) {
                break;
            }
        }
        Ok(hits)
    })
}

/// Scans an arbitrary ordered Local storage source.
pub fn search_local_session_storages<'a>(
    storages: &'a [Rc<dyn LocalSessionStorage>],
    text: &'a str,
    options: SessionSearchOptions,
    cancellation: CancellationToken,
) -> LocalBoxFuture<'a, Result<Vec<SessionSearchHit>, SessionError>> {
    Box::pin(async move {
        let normalized = normalize_query(text, &options);
        let Some(normalized) = normalized else {
            return Ok(Vec::new());
        };
        let mut hits = Vec::new();
        let mut seen = BTreeSet::new();
        for storage in storages {
            check_cancelled(&cancellation)?;
            let metadata = storage.metadata().await?;
            if !seen.insert(metadata.session_id.clone()) {
                return Err(duplicate_session(&metadata.session_id));
            }
            let state = storage.load_state().await?;
            scan_state(
                &metadata.session_id,
                &state,
                &normalized,
                &options,
                &cancellation,
                &mut hits,
            )?;
            if reached_limit(&hits, options.limit) {
                break;
            }
        }
        Ok(hits)
    })
}

/// Lists, opens, and scans every matching Send repository session.
pub fn search_session_repository<'a>(
    repository: &'a dyn SessionRepository,
    text: &'a str,
    options: SessionSearchOptions,
    cancellation: CancellationToken,
) -> SendBoxFuture<'a, Result<Vec<SessionSearchHit>, SessionError>> {
    Box::pin(async move {
        let metadata = repository.list(Default::default()).await?;
        let mut storages = Vec::with_capacity(metadata.len());
        for item in metadata {
            check_cancelled(&cancellation)?;
            storages.push(repository.open(&item.session_id).await?);
        }
        search_session_storages(&storages, text, options, cancellation).await
    })
}

/// Lists, opens, and scans every matching Local repository session.
pub fn search_local_session_repository<'a>(
    repository: &'a dyn LocalSessionRepository,
    text: &'a str,
    options: SessionSearchOptions,
    cancellation: CancellationToken,
) -> LocalBoxFuture<'a, Result<Vec<SessionSearchHit>, SessionError>> {
    Box::pin(async move {
        let metadata = repository.list(Default::default()).await?;
        let mut storages = Vec::with_capacity(metadata.len());
        for item in metadata {
            check_cancelled(&cancellation)?;
            storages.push(repository.open(&item.session_id).await?);
        }
        search_local_session_storages(&storages, text, options, cancellation).await
    })
}

fn normalize_query(text: &str, options: &SessionSearchOptions) -> Option<String> {
    let normalized = text.trim().to_lowercase();
    (!normalized.is_empty()
        && options.limit != Some(0)
        && options
            .entry_kinds
            .as_ref()
            .is_none_or(|kinds| !kinds.is_empty()))
    .then_some(normalized)
}

fn scan_state(
    session_id: &SessionId,
    state: &crate::SessionState,
    query: &str,
    options: &SessionSearchOptions,
    cancellation: &CancellationToken,
    hits: &mut Vec<SessionSearchHit>,
) -> Result<(), SessionError> {
    for entry in state.entries_in_sequence_order() {
        check_cancelled(cancellation)?;
        if options
            .entry_kinds
            .as_ref()
            .is_some_and(|kinds| !kinds.contains(&SessionEntryKind::of(entry)))
        {
            continue;
        }
        let mut snippet = serde_json::to_string(entry).map_err(|error| {
            SessionError::new(
                SessionErrorKind::Corruption,
                format!("failed to project searchable session entry: {error}"),
            )
        })?;
        if let Some(label) = state.label(entry.id()) {
            snippet.push(' ');
            snippet.push_str(label);
        }
        if snippet.to_lowercase().contains(query) {
            hits.push(SessionSearchHit {
                session_id: session_id.clone(),
                entry_id: entry.id().clone(),
                timestamp: entry.base().timestamp,
                snippet,
            });
            if reached_limit(hits, options.limit) {
                break;
            }
        }
    }
    Ok(())
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), SessionError> {
    if cancellation.is_cancelled() {
        Err(SessionError::new(
            SessionErrorKind::Cancelled,
            "session search was cancelled",
        ))
    } else {
        Ok(())
    }
}

fn reached_limit(hits: &[SessionSearchHit], limit: Option<usize>) -> bool {
    limit.is_some_and(|limit| hits.len() >= limit)
}

fn duplicate_session(id: &SessionId) -> SessionError {
    SessionError::new(
        SessionErrorKind::Corruption,
        format!("duplicate session id in scanning source: {id}"),
    )
}
