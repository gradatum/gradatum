//! Erreurs propres au crate `gradatum-vault`.
//!
//! `VaultError` encapsule les erreurs des couches sous-jacentes
//! et ajoute des variantes spécifiques au vault (opération lifecycle, override, drift).
//!
//! Règle Rust-grade : pas de `Box<dyn Error>` en lib publique — typage fort via `thiserror`.

use thiserror::Error;

/// Erreur top-level de `gradatum-vault`.
///
/// Produite par toutes les opérations publiques du vault (create/open, lifecycle,
/// overrides, drift, cache effective_note).
#[derive(Debug, Error)]
pub enum VaultError {
    /// Erreur provenant de `gradatum-core` (note not found, drift, validation, etc.).
    #[error("core: {0}")]
    Core(#[from] gradatum_core::error::GradatumError),

    /// Erreur de stockage (OpenDAL, filesystem, NFS check).
    ///
    /// Note : `StorageError` peut lui-même encapsuler `GradatumError::VaultOnNfs` (C11).
    #[error("storage: {0}")]
    Storage(String),

    /// Erreur de sérialisation/parsing Markdown.
    #[error("markdown: {0}")]
    Markdown(String),
}

impl From<gradatum_storage::StorageError> for VaultError {
    fn from(e: gradatum_storage::StorageError) -> Self {
        VaultError::Storage(e.to_string())
    }
}

impl From<gradatum_markdown::MarkdownError> for VaultError {
    fn from(e: gradatum_markdown::MarkdownError) -> Self {
        VaultError::Markdown(e.to_string())
    }
}
