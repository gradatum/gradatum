//! Noop implementation — always returns `PendingReview` with confidence 0.0.
//!
//! Useful in tests and as a safe fallback when no backend is configured.

use async_trait::async_trait;

use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;

use crate::chat_trait::{Chat, ChatBackend, CuratorContext, CuratorVerdict};
use crate::error::ChatError;

/// Noop backend — no classification logic.
///
/// Always returns `PendingReview` with a confidence of 0.0.
/// Suitable as a safe default when the curator is not configured.
pub struct Noop;

#[async_trait]
impl Chat for Noop {
    async fn classify_curator(
        &self,
        _note: &Note,
        _context: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError> {
        Ok(CuratorVerdict {
            proposed_status: NoteStatus::PendingReview,
            confidence: 0.0,
            reason: "noop classifier".into(),
            backend: ChatBackend::Noop,
        })
    }

    fn backend_kind(&self) -> ChatBackend {
        ChatBackend::Noop
    }
}
