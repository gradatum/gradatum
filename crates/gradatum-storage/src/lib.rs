//! # gradatum-storage
//!
//! Storage trait abstraction avec backends OpenDAL (filesystem, S3, Azure Blob).
//!
//! ## Trait principal
//!
//! [`Storage`] — primitives async Read/Write/List/Delete/Stat/Exists.
//!
//! ## Implémentations
//!
//! - [`FileStorage`] — backend OpenDAL filesystem (feature `fs`, activée par défaut).
//! - Backend S3 (feature `s3`) — Phase 2+, non implémenté.
//! - Backend Azure Blob (feature `azblob`) — Phase 2+, non implémenté.
//!
//! ## Guard NFS (caveat C11)
//!
//! [`ensure_local_filesystem`] vérifie via `statfs(2)` que le chemin fourni n'est pas
//! sur un montage NFS. Appelé automatiquement par `FileStorage::new()`.
//! Retourne `Err(StorageError::Core(GradatumError::VaultOnNfs))` si NFS détecté.
//!
//! ## Stabilité
//!
//! `0.x` — pas de garantie de stabilité API. Voir
//! [`RELEASE-POLICY.md`](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Ref
//!
//! - Spec §0.3 C11
//! - Plan T10 `docs/superpowers/plans/2026-05-04-phase1-backend-plan.md`

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod error;
pub mod nfs_check;
pub mod storage_trait;

#[cfg(feature = "fs")]
pub mod file;

// Re-exports publics.
pub use error::StorageError;
pub use nfs_check::ensure_local_filesystem;
pub use storage_trait::{Storage, StorageEntry};

#[cfg(feature = "fs")]
pub use file::FileStorage;

/// Version de la crate (depuis `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
