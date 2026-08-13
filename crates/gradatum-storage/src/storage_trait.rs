//! `Storage` trait — abstraction over primitive storage operations.
//!
//! All operations are asynchronous and path-based.
//! Paths are relative to the root configured in each implementation.
//!
//! ## Provided implementations
//!
//! - [`crate::FileStorage`] — generic OpenDAL wrapper. `FileStorage::new` builds a local
//!   filesystem backend (feature `fs`, enabled by default).
//! - S3 object storage (feature `s3`) — served by the same wrapper over an S3 operator,
//!   built from configuration via [`crate::build_storage`].
//! - GCS / Azure Blob (features `gcs`, `azure`) — declared, not yet wired in the factory.
//!
//! ## Path contract
//!
//! Paths passed to trait methods are always relative to the storage root.
//! The separator is `/` (Unix). Absolute paths are rejected.

use async_trait::async_trait;

use crate::error::StorageError;

/// Entry returned by `list` and `stat` — metadata for a stored object.
#[derive(Debug, Clone, PartialEq)]
pub struct StorageEntry {
    /// Relative path from the storage root.
    pub path: String,
    /// Size in bytes. `0` for directories.
    pub size: u64,
    /// Last-modified timestamp in Unix epoch milliseconds.
    /// `None` if the backend does not provide this information.
    pub last_modified: Option<i64>,
    /// `true` if the entry is a directory (prefix/folder).
    pub is_dir: bool,
}

/// Storage abstraction trait — async primitives: Read/Write/List/Delete/Stat.
///
/// All implementations must be `Send + Sync` to be usable
/// in async multi-threaded contexts (Tokio).
///
/// ## Errors
///
/// - Entry absent → `StorageError::NotFound`
/// - Invalid path → `StorageError::InvalidPath`
/// - Backend error → `StorageError::OpenDal`
#[async_trait]
pub trait Storage: Send + Sync {
    /// Reads the raw content of the object at relative path `path`.
    ///
    /// # Errors
    ///
    /// - `StorageError::NotFound` if `path` does not exist.
    /// - `StorageError::OpenDal` on read error.
    async fn read(&self, path: &str) -> Result<Vec<u8>, StorageError>;

    /// Writes `content` to `path`, creating intermediate directories as needed.
    ///
    /// # Side effects
    ///
    /// Silently overwrites any existing content if `path` already exists.
    ///
    /// # Errors
    ///
    /// - `StorageError::OpenDal` on write error or insufficient permissions.
    async fn write(&self, path: &str, content: &[u8]) -> Result<(), StorageError>;

    /// Deletes the object at `path`.
    ///
    /// # Errors
    ///
    /// - `StorageError::NotFound` if `path` does not exist.
    /// - `StorageError::OpenDal` on deletion error.
    async fn delete(&self, path: &str) -> Result<(), StorageError>;

    /// Lists entries whose path starts with `prefix`.
    ///
    /// `prefix` may be a directory (e.g. `"notes/"`) or an arbitrary string.
    /// Directories themselves may appear in the result depending on the backend.
    ///
    /// # Errors
    ///
    /// - `StorageError::OpenDal` on directory read error.
    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, StorageError>;

    /// Returns metadata for the object at `path`.
    ///
    /// # Errors
    ///
    /// - `StorageError::NotFound` if `path` does not exist.
    /// - `StorageError::OpenDal` on stat error.
    async fn stat(&self, path: &str) -> Result<StorageEntry, StorageError>;

    /// Returns `true` if an object exists at `path`, `false` otherwise.
    ///
    /// Optimised equivalent of `stat(path).is_ok()` — may avoid an allocation depending on the backend.
    ///
    /// # Errors
    ///
    /// - `StorageError::OpenDal` on unexpected backend error (not NotFound).
    async fn exists(&self, path: &str) -> Result<bool, StorageError>;

    /// Creates the directory at relative path `path` (idempotent).
    ///
    /// `path` must end with `/` (OpenDAL `create_dir` convention).
    /// The operation is idempotent — no error if the directory already exists.
    ///
    /// # Errors
    ///
    /// - `StorageError::OpenDal` if the backend does not support `create_dir`.
    /// - `StorageError::OpenDal` on filesystem error (permissions, etc.).
    async fn create_dir(&self, path: &str) -> Result<(), StorageError>;
}
