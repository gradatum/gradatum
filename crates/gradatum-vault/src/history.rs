//! Note version history — `NoteHistoryEntry`.
//!
//! `NoteHistoryEntry` represents an atomic transition between two versions.
//! The `diff_text` field is currently always an empty string — the unified diff
//! (textual representation of changes between two versions) will be computed
//! via the `similar` library (Myers diff) in a future release.
//!
//! The copy-on-write history is stored at `.history/<note_id>/<ts_ms>.md` on disk.
//! SQL persistence in a dedicated `note_history` table is deferred.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use gradatum_core::author::AuthorRef;
use gradatum_core::identity::{NoteId, NoteVersion};

/// History entry for a note version transition.
///
/// Represents an atomic transition between two note versions.
/// Unified diff and SQL persistence are deferred.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteHistoryEntry {
    /// Identifier of the note involved.
    pub note_id: NoteId,

    /// Version before the transition.
    pub from_version: NoteVersion,

    /// Version after the transition.
    pub to_version: NoteVersion,

    /// Unified text diff between the two versions.
    ///
    /// Currently always the empty string `""`.
    /// Unified-format diff via `similar::TextDiff` is deferred.
    pub diff_text: String,

    /// Timestamp of the transition.
    pub committed_at: DateTime<Utc>,

    /// Author of the transition (human, agent, or system).
    pub committed_by: AuthorRef,

    /// Optional message describing the change.
    ///
    /// Analogous to a Git commit message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,

    /// Correlation identifier for tracing multi-note operations.
    ///
    /// Useful for correlating a batch editing session or a bulk import.
    /// `None` if not provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<Ulid>,
}

impl NoteHistoryEntry {
    /// Creates a history entry with an empty diff and no `correlation_id`.
    pub fn new(
        note_id: NoteId,
        from_version: NoteVersion,
        to_version: NoteVersion,
        committed_by: AuthorRef,
        commit_message: Option<String>,
    ) -> Self {
        Self {
            note_id,
            from_version,
            to_version,
            diff_text: String::new(),
            committed_at: Utc::now(),
            committed_by,
            commit_message,
            correlation_id: None,
        }
    }
}
