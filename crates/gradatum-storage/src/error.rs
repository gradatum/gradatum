//! Erreurs de la couche storage.
//!
//! Toutes les erreurs sont typées via `thiserror` — pas de `Box<dyn Error>` en lib publique.
//! Règle Rust-grade : typage fort, propagation explicite.

use std::path::PathBuf;
use thiserror::Error;

/// Erreur produite par les opérations de stockage (`Storage` trait et implémentations).
///
/// `StorageError::Core` encapsule les erreurs `GradatumError` remontant du check NFS (C11).
#[derive(Debug, Error)]
pub enum StorageError {
    /// Erreur I/O générique (statfs, lecture, écriture, permissions).
    #[error("io: {0}")]
    Io(String),

    /// Ressource introuvable au chemin indiqué.
    #[error("not found: {0}")]
    NotFound(String),

    /// Chemin invalide (non-UTF-8 ou hors racine autorisée).
    #[error("invalid path: {0:?}")]
    InvalidPath(PathBuf),

    /// Erreur retournée par le backend OpenDAL.
    #[error("opendal: {0}")]
    OpenDal(String),

    /// Erreur provenant de `gradatum-core` (ex. `GradatumError::VaultOnNfs` — caveat C11).
    #[error("core: {0}")]
    Core(#[from] gradatum_core::error::GradatumError),
}
