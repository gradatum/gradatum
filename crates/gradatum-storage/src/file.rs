//! `FileStorage` — OpenDAL-backed implementation of the `Storage` trait.
//!
//! Despite the historical `FileStorage` name, this type is the **generic OpenDAL
//! wrapper**: every `Storage` method only delegates to the inner `opendal::Operator`,
//! so it serves any backend. `FileStorage::new` builds a local filesystem operator;
//! `crate::build_storage` builds an object operator (S3) via `from_operator`.
//! (A rename to a backend-neutral name is deferred — see the crate root.)
//!
//! ## How it works
//!
//! Delegates all I/O operations to an `opendal::Operator`. For a filesystem backend
//! the root is fixed at construction time; for an object backend (S3) the location
//! comes from configuration.
//!
//!
//! ## Security — path traversal guard
//!
//! OpenDAL does not natively reject `..` components. Each operation calls
//! `validate_relative_path()` on entry — a mandatory defense-in-depth layer enforcing
//! the "confined Storage abstraction" contract, applied to **every** backend,
//! object stores included.
//!
//! ## Features
//!
//! Available via the `fs` feature (enabled by default).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
#[cfg(feature = "fs")]
use opendal::services;
use opendal::{EntryMode, Operator};
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
    /// # Errors
    ///
    /// - `StorageError::InvalidPath` — path is not valid UTF-8.
    /// - `StorageError::OpenDal` — `Operator` construction failed.
    #[cfg(feature = "fs")]
    pub fn new(root: &Path) -> Result<Self, StorageError> {
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

    /// Wraps a pre-built OpenDAL [`Operator`] (any backend) as a `Storage`.
    ///
    /// This is the generic OpenDAL wrapper: despite the historical `FileStorage`
    /// name, all seven `Storage` methods only delegate to the inner `Operator` and
    /// are backend-agnostic. Used by [`crate::build_storage`] for object backends,
    /// where the operator is produced from configuration rather than from a local path.
    ///
    /// `root` is retained for diagnostics only (`root()`) and is never used for I/O —
    /// for an object backend it carries the configured prefix, not a filesystem path.
    #[must_use]
    pub(crate) fn from_operator(op: Operator, root: PathBuf) -> Self {
        Self { op, root }
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
            // Note: this `NotFound` arm is reachable on a filesystem backend, but is
            // unreachable on S3 — object-store delete is idempotent and returns `Ok`
            // for an absent key. Kept for the filesystem backend; not dead code there.
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

    /// Ensures the directory at `path` exists (idempotent).
    ///
    /// `path` must end with `/` (OpenDAL requirement).
    ///
    /// On a **filesystem** backend this delegates to `Operator::create_dir`. On an
    /// **object** backend (S3, …) there are no real directories: OpenDAL rejects
    /// `create_dir` with `ErrorKind::Unsupported`. Because the trait contract is only
    /// "ensure this directory exists", and on an object store that assurance holds
    /// with no operation (a note write creates its key directly), `Unsupported` is
    /// mapped to success. Every other error still propagates — a permission denial or
    /// a network failure is never masked.
    ///
    /// # Errors
    ///
    /// - `StorageError::InvalidPath` — absolute path or path containing `..`.
    /// - `StorageError::OpenDal` — a real backend error (permissions, network, …).
    ///   `Unsupported` is **not** an error here.
    #[instrument(skip(self), fields(path))]
    async fn create_dir(&self, path: &str) -> Result<(), StorageError> {
        validate_relative_path(path)?;
        match self.op.create_dir(path).await {
            Ok(()) => Ok(()),
            // Object stores have no directories: absence of the operation is the
            // correct semantics, not a failure.
            Err(e) if e.kind() == opendal::ErrorKind::Unsupported => Ok(()),
            Err(e) => Err(StorageError::OpenDal(e.to_string())),
        }
    }
}
