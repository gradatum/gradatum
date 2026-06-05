//! Erreurs du crate `gradatum-curator`.

use thiserror::Error;

/// Erreurs possibles lors d'une décision de curation.
///
/// Note : le workflow [`crate::workflow::Curator::decide`] ne retourne PAS
/// d'erreur — toutes les erreurs internes sont absorbées en `CuratorDecision`
/// avec `fallback_applied = true`. Ce type est réservé aux fonctions utilitaires
/// du crate qui peuvent échouer explicitement.
#[derive(Debug, Error)]
pub enum CuratorError {
    /// Erreur propagée depuis le backend Chat.
    #[error("chat: {0}")]
    Chat(#[from] gradatum_chat::ChatError),
}
