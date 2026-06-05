//! Implémentation Noop — retourne toujours `PendingReview` confidence 0.0.
//!
//! Utile en tests et comme fallback sécurisé quand aucun backend n'est configuré.

use async_trait::async_trait;

use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;

use crate::chat_trait::{Chat, ChatBackend, CuratorContext, CuratorVerdict};
use crate::error::ChatError;

/// Backend noop — aucune logique de classification.
///
/// Retourne systématiquement `PendingReview` avec une confiance de 0.0.
/// Idéal comme valeur par défaut safe lorsque le curator n'est pas configuré.
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
