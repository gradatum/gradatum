//! Trait `Chat` and shared contract types for all implementations.
//!
//! ## Isolation invariant
//!
//! `gradatum-chat` does not communicate directly with disk, SQLite, or the
//! scheduler. Its only side effect is an optional network call (`HttpChat`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;

use crate::error::ChatError;

/// Note classifier for the curator.
///
/// Implemented by:
/// - [`crate::heuristic::Heuristic`] — offline, regex/keyword
/// - [`crate::http::HttpChat`] — reqwest OpenAI-compat
/// - [`crate::noop::Noop`] — no-op, useful in tests
/// - [`crate::circuit_breaker::CircuitBreakerChat`] — decorator pattern
#[async_trait]
pub trait Chat: Send + Sync {
    /// Classifies a note to determine whether it should be admitted into the vault.
    ///
    /// Returns a `CuratorVerdict` with the proposed status, a confidence score
    /// in `0.0..=1.0`, and a textual reason.
    ///
    /// # Side effects
    ///
    /// - `Heuristic`: none.
    /// - `HttpChat`: network call to an OpenAI-compatible endpoint.
    /// - `CircuitBreakerChat`: atomic update of failure counters.
    async fn classify_curator(
        &self,
        note: &Note,
        context: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError>;

    /// Identifies the backend type without downcasting.
    fn backend_kind(&self) -> ChatBackend;
}

/// Optional context provided by the caller to improve classification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CuratorContext {
    /// IDs of similar notes already present in the vault (deduplication hint).
    pub similar_note_ids: Vec<String>,
    /// Tags of the current vault (thematic context).
    pub vault_tags: Vec<String>,
}

/// Verdict returned by a classifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorVerdict {
    /// Proposed status for the note after classification.
    pub proposed_status: NoteStatus,
    /// Confidence in the decision — range `0.0..=1.0`.
    pub confidence: f32,
    /// Textual explanation of the decision (logged, never exposed in the public API).
    pub reason: String,
    /// Backend that produced this verdict.
    pub backend: ChatBackend,
}

/// Backend discriminant — avoids downcasting in logs and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatBackend {
    /// Offline heuristic classifier (regex/keywords).
    Heuristic,
    /// OpenAI-compatible HTTP backend.
    Http,
    /// Noop backend — always returns `PendingReview` with zero confidence.
    Noop,
}
