//! # gradatum-storage
//!
//! Storage trait abstraction with OpenDAL backends (filesystem, S3, Azure Blob).
//!
//! ## Core trait
//!
//! [`Storage`] — async primitives: Read/Write/List/Delete/Stat/Exists.
//!
//! ## Implementations
//!
//! - [`FileStorage`] — OpenDAL filesystem backend (feature `fs`, enabled by default).
//! - S3 / GCS / Azure Blob backends (features `s3`, `gcs`, `azure`) — planned: enabling a feature
//!   pulls the matching OpenDAL service, but no `Storage` implementation is provided yet.
//!
//! ## NFS guard
//!
//! [`ensure_local_filesystem`] uses `statfs(2)` to verify that the given path does not
//! reside on an NFS mount. Called automatically by `FileStorage::new()`.
//! Returns `Err(StorageError::Core(GradatumError::VaultOnNfs))` if NFS is detected.
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee. See
//! [`RELEASE-POLICY.md`](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod error;
pub mod nfs_check;
pub mod storage_trait;

#[cfg(feature = "fs")]
pub mod file;

// Public re-exports.
pub use error::StorageError;
pub use nfs_check::ensure_local_filesystem;
pub use storage_trait::{Storage, StorageEntry};

#[cfg(feature = "fs")]
pub use file::FileStorage;

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
