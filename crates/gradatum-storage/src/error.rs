//! Error types for the storage layer.
//!
//! All errors are typed via `thiserror` — no `Box<dyn Error>` in a public library.
//! Strong typing with explicit error propagation throughout.

use std::path::PathBuf;
use thiserror::Error;

/// Error produced by storage operations (`Storage` trait and implementations).
///
/// `StorageError::Core` wraps `GradatumError` values propagated from the core crate.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StorageError {
    /// Generic I/O error (read, write, permissions).
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

    /// Invalid or unsupported storage configuration.
    ///
    /// Produced by the storage factory when the configured service is unknown, when
    /// its Cargo feature is not enabled in this build, or when a required connection
    /// parameter (e.g. `bucket` for `s3`) is missing. The message names what is wrong
    /// and **never** contains a credential — gradatum reads no secret.
    #[error("storage config: {0}")]
    ConfigInvalid(String),

    /// Write refused by a storage policy layer.
    ///
    /// Emitted by a `Storage` decorator that gates writes — notably the note-write
    /// convergence guard, which refuses to persist a note `.md` file coming
    /// from a path that does not converge with the write funnel (index + drift
    /// footprint). The write **fails outright** rather than succeeding silently and
    /// producing an orphan file on disk. The message names the rejected path and the
    /// sanctioned alternative; it **never** contains a credential.
    #[error("write rejected: {0}")]
    WriteRejected(String),

    /// Error originating from `gradatum-core`.
    #[error("core: {0}")]
    Core(#[from] gradatum_core::error::GradatumError),
}
