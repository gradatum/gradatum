//! `FileStorage` — OpenDAL filesystem implementation of the `Storage` trait.
//!
//! ## How it works
//!
//! Delegates all I/O operations to an `opendal::Operator` configured with
//! the `services::Fs` backend. The root is fixed at construction time.
//!
//! ## NFS guard
//!
//! `FileStorage::new()` calls `ensure_local_filesystem(root)` **before**
//! constructing the `Operator`. If the path resides on NFS, construction fails
//! with `StorageError::Core(GradatumError::VaultOnNfs)`.
//!
//! ## Security — path traversal guard
//!
//! OpenDAL Fs 0.58 does not natively reject `..` components. Each operation
//! calls `validate_relative_path()` on entry — a mandatory defense-in-depth layer
//! enforcing the "confined Storage abstraction" contract (networked S3/GCS backends
//! are not yet implemented).
//!
//! ## Features
//!
//! Available via the `fs` feature (enabled by default).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use opendal::{EntryMode, Operator, services};
use tracing::instrument;

use crate::error::StorageError;
use crate::storage_trait::{Storage, StorageEntry};

/// `Storage` implementation backed by the OpenDAL filesystem operator.
///
/// Thread-safe — `Operator` is `Clone + Send + Sync` internally.
pub struct FileStorage {
    /// OpenDAL operator configured at the root.
    op: Operator,
    /// Absolute path of the root (retained for diagnostics and `root()`).
    root: PathBuf,
}

/// Validates that a relative path contains no `..` components and is not absolute.
///
/// OpenDAL Fs 0.58 does not natively reject `..` components — this guard
/// is the sole barrier against path traversal outside the configured root.
///
/// # Errors
///
/// - `StorageError::InvalidPath` — path is absolute (starts with `/`) or contains `..`.
fn validate_relative_path(path: &str) -> Result<(), StorageError> {
    // Reject absolute paths (starting with `/`).
    if path.starts_with('/') {
        return Err(StorageError::InvalidPath(PathBuf::from(path)));
    }
    // Reject any `..` component to prevent path traversal outside the configured root.
    // Explicit check rather than relying on OpenDAL: FsBackend::read/write/etc. performs
    // `root.join(path)` without post-join canonicalisation — `../x` would escape the root.
    for component in std::path::Path::new(path).components() {
        if component == std::path::Component::ParentDir {
            return Err(StorageError::InvalidPath(PathBuf::from(path)));
        }
    }
    Ok(())
}

impl FileStorage {
    /// Constructs a `FileStorage` rooted at `root`.
    ///
    /// ## NFS guard
    ///
    /// Calls `ensure_local_filesystem(root)` first. Returns
    /// `Err(StorageError::Core(GradatumError::VaultOnNfs))` if NFS is detected.
    ///
    /// # Errors
    ///
    /// - `StorageError::Core(VaultOnNfs)` — path resides on NFS.
    /// - `StorageError::InvalidPath` — path is not valid UTF-8.
    /// - `StorageError::OpenDal` — `Operator` construction failed.
    pub fn new(root: &Path) -> Result<Self, StorageError> {
        // NFS guard — must run before any construction.
        crate::nfs_check::ensure_local_filesystem(root)?;

        let root_str = root
            .to_str()
            .ok_or_else(|| StorageError::InvalidPath(root.to_path_buf()))?;

        let builder = services::Fs::default().root(root_str);
        // OpenDAL 0.58 : `Operator::new(builder)` rend `Result<Operator>` directement
        // (plus d'intermédiaire `OperatorBuilder`/`.finish()` comme en <= 0.57). Aucune
        // couche (`.layer(...)`) n'est ajoutée ici, donc le `.finish()` disparaît sans perte.
        let op = Operator::new(builder).map_err(|e| StorageError::OpenDal(e.to_string()))?;

        Ok(Self {
            op,
            root: root.to_path_buf(),
        })
    }

    /// Returns the absolute path of the configured root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl Storage for FileStorage {
    /// Reads the content of the file at relative path `path`.
    ///
    /// `path` is relative to the storage root.
    ///
    /// # Errors
    ///
    /// - `StorageError::InvalidPath` — absolute path or path containing `..`.
    #[instrument(skip(self), fields(path))]
    async fn read(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        validate_relative_path(path)?;
        self.op
            .read(path)
            .await
            .map(|buf| buf.to_vec())
            .map_err(|e| {
                if e.kind() == opendal::ErrorKind::NotFound {
                    StorageError::NotFound(path.to_owned())
                } else {
                    StorageError::OpenDal(e.to_string())
                }
            })
    }

    /// Writes `content` to relative path `path`.
    ///
    /// Intermediate directories are created automatically by OpenDAL/Fs.
    ///
    /// # Errors
    ///
    /// - `StorageError::InvalidPath` — absolute path or path containing `..`.
    #[instrument(skip(self, content), fields(path, bytes = content.len()))]
    async fn write(&self, path: &str, content: &[u8]) -> Result<(), StorageError> {
        validate_relative_path(path)?;
        // OpenDAL >= 0.56 renvoie la `Metadata` de l'objet écrit ; le contrat
        // `Storage::write` reste `()` — la métadonnée est délibérément ignorée.
        self.op
            .write(path, content.to_vec())
            .await
            .map(|_meta| ())
            .map_err(|e| StorageError::OpenDal(e.to_string()))
    }

    /// Deletes the file at relative path `path`.
    ///
    /// # Errors
    ///
    /// - `StorageError::InvalidPath` — absolute path or path containing `..`.
    #[instrument(skip(self), fields(path))]
    async fn delete(&self, path: &str) -> Result<(), StorageError> {
        validate_relative_path(path)?;
        self.op.delete(path).await.map_err(|e| {
            if e.kind() == opendal::ErrorKind::NotFound {
                StorageError::NotFound(path.to_owned())
            } else {
                StorageError::OpenDal(e.to_string())
            }
        })
    }

    /// Lists entries whose path starts with `prefix`.
    ///
    /// Returns a flat list (per-entry non-recursive, but the prefix itself is scanned recursively).
    /// Directories are included if returned by the backend.
    ///
    /// # Errors
    ///
    /// - `StorageError::InvalidPath` — absolute path or path containing `..`.
    #[instrument(skip(self), fields(prefix))]
    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, StorageError> {
        validate_relative_path(prefix)?;
        // `list_with(prefix).recursive(true)` for a recursive scan (equivalent to find).
        // Without `.recursive(true)`, only the immediate level is returned.
        let entries = self
            .op
            .list_with(prefix)
            .recursive(true)
            .await
            .map_err(|e| StorageError::OpenDal(e.to_string()))?;

        let result = entries
            .into_iter()
            .map(|e| {
                let meta = e.metadata();
                let is_dir = matches!(meta.mode(), EntryMode::DIR);
                let size = if is_dir { 0 } else { meta.content_length() };
                // OpenDAL >= 0.56 : `last_modified()` rend un newtype jiff
                // `opendal::raw::Timestamp` ; `.into_inner()` donne le `jiff::Timestamp`
                // sous-jacent, puis `.as_millisecond()` l'epoch en millisecondes.
                let last_modified = meta
                    .last_modified()
                    .map(|ts| ts.into_inner().as_millisecond());
                StorageEntry {
                    path: e.path().to_owned(),
                    size,
                    last_modified,
                    is_dir,
                }
            })
            .collect();

        Ok(result)
    }

    /// Returns metadata for the object at `path`.
    ///
    /// # Errors
    ///
    /// - `StorageError::InvalidPath` — absolute path or path containing `..`.
    #[instrument(skip(self), fields(path))]
    async fn stat(&self, path: &str) -> Result<StorageEntry, StorageError> {
        validate_relative_path(path)?;
        let meta = self.op.stat(path).await.map_err(|e| {
            if e.kind() == opendal::ErrorKind::NotFound {
                StorageError::NotFound(path.to_owned())
            } else {
                StorageError::OpenDal(e.to_string())
            }
        })?;

        let is_dir = matches!(meta.mode(), EntryMode::DIR);
        let size = if is_dir { 0 } else { meta.content_length() };
        // OpenDAL >= 0.56 : newtype jiff `opendal::raw::Timestamp` — cf. `list`.
        let last_modified = meta
            .last_modified()
            .map(|ts| ts.into_inner().as_millisecond());

        Ok(StorageEntry {
            path: path.to_owned(),
            size,
            last_modified,
            is_dir,
        })
    }

    /// Returns `true` if an object exists at `path`, `false` otherwise.
    ///
    /// # Errors
    ///
    /// - `StorageError::InvalidPath` — absolute path or path containing `..`.
    #[instrument(skip(self), fields(path))]
    async fn exists(&self, path: &str) -> Result<bool, StorageError> {
        validate_relative_path(path)?;
        self.op
            .exists(path)
            .await
            .map_err(|e| StorageError::OpenDal(e.to_string()))
    }

    /// Creates the directory at `path` (idempotent).
    ///
    /// Delegates to `Operator::create_dir` — a native OpenDAL operation.
    /// `path` must end with `/` (OpenDAL requirement).
    ///
    /// # Errors
    ///
    /// - `StorageError::InvalidPath` — absolute path or path containing `..`.
    #[instrument(skip(self), fields(path))]
    async fn create_dir(&self, path: &str) -> Result<(), StorageError> {
        validate_relative_path(path)?;
        self.op
            .create_dir(path)
            .await
            .map_err(|e| StorageError::OpenDal(e.to_string()))
    }
}
