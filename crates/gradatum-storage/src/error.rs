//! Error types for the storage layer.
//!
//! All errors are typed via `thiserror` — no `Box<dyn Error>` in a public library.
//! Strong typing with explicit error propagation throughout.

use std::path::PathBuf;
use thiserror::Error;

/// Error produced by storage operations (`Storage` trait and implementations).
///
/// `StorageError::Core` wraps `GradatumError` values propagated from the NFS guard.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Generic I/O error (statfs, read, write, permissions).
    #[error("io: {0}")]
    Io(String),

    /// Resource not found at the given path.
    #[error("not found: {0}")]
    NotFound(String),

    /// Invalid path (non-UTF-8 or outside the allowed root).
    #[error("invalid path: {0:?}")]
    InvalidPath(PathBuf),

    /// Error returned by the OpenDAL backend.
    #[error("opendal: {0}")]
    OpenDal(String),

    /// Error originating from `gradatum-core` (e.g. `GradatumError::VaultOnNfs`).
    #[error("core: {0}")]
    Core(#[from] gradatum_core::error::GradatumError),
}
