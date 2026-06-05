# gradatum-storage

> Storage trait abstraction with OpenDAL backends (filesystem, S3, Azure Blob) and NFS rejection guard.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Trait

```rust
/// Storage abstraction — async read/write/list/delete/stat primitives.
#[async_trait]
pub trait Storage: Send + Sync {
    async fn read(&self, path: &str) -> Result<Bytes, StorageError>;
    async fn write(&self, path: &str, data: Bytes) -> Result<(), StorageError>;
    async fn delete(&self, path: &str) -> Result<(), StorageError>;
    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, StorageError>;
    async fn exists(&self, path: &str) -> Result<bool, StorageError>;
    async fn stat(&self, path: &str) -> Result<StorageEntry, StorageError>;
}
```

### Implementations

```rust
/// Filesystem backend via OpenDAL (feature = "fs", enabled by default).
pub struct FileStorage { ... }

impl FileStorage {
    /// Create a new FileStorage rooted at `root`.
    /// Returns Err if root is on an NFS mount (caveat C11).
    pub fn new(root: &Path) -> Result<Self, StorageError>
}
```

### Functions

```rust
/// Verify via statfs(2) that `path` is not on an NFS mount.
/// Called automatically by FileStorage::new().
pub fn ensure_local_filesystem(path: &Path) -> Result<(), StorageError>
```

### Types

```rust
pub struct StorageEntry {
    pub path: String,
    pub size: u64,
    pub last_modified: Option<DateTime<Utc>>,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    Io(std::io::Error),
    Core(GradatumError),
    NfsMountDetected { path: PathBuf },
    Backend(String),
}
```

## Feature flags

| Feature | Description | Default |
|---|---|---|
| `fs` | OpenDAL filesystem backend (`FileStorage`) | enabled |
| `s3` | S3 backend (Phase 2+, not yet implemented) | disabled |
| `azblob` | Azure Blob backend (Phase 2+, not yet implemented) | disabled |

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0