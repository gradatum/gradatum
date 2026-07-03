# gradatum-storage

> Storage trait abstraction with OpenDAL filesystem backend and NFS rejection guard.

**Status**: 0.7.6 — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-storage` defines the `Storage` async trait and provides a filesystem implementation
backed by OpenDAL. S3 and Azure Blob backends are planned for a future release.

The NFS guard (`ensure_local_filesystem`) uses `statfs(2)` to verify that the vault root
is not on an NFS mount before opening. This prevents silent data loss on network
partitions. `FileStorage::new()` calls this check automatically.

## Usage

```toml
[dependencies]
gradatum-storage = "0.7.6"
```

```rust
use gradatum_storage::FileStorage;
use std::path::Path;

let storage = FileStorage::new(Path::new("/var/lib/gradatum/vault"))?;
let content = storage.read("decisions/my-note.md").await?;
```

## Trait

```rust
#[async_trait]
pub trait Storage: Send + Sync {
    async fn read(&self, path: &str) -> Result<Vec<u8>, StorageError>;
    async fn write(&self, path: &str, content: &[u8]) -> Result<(), StorageError>;
    async fn delete(&self, path: &str) -> Result<(), StorageError>;
    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, StorageError>;
    async fn exists(&self, path: &str) -> Result<bool, StorageError>;
    async fn stat(&self, path: &str) -> Result<StorageEntry, StorageError>;
    async fn create_dir(&self, path: &str) -> Result<(), StorageError>;
}
```

## Feature Flags

| Feature | Default | Description |
|---|---|---|
| `fs` | yes | Filesystem backend via OpenDAL |
| `s3` | no | Enables the OpenDAL S3 service dependency (no `Storage` impl included yet) |
| `gcs` | no | Enables the OpenDAL GCS service dependency (no `Storage` impl included yet) |
| `azure` | no | Enables the OpenDAL Azure Blob service dependency (no `Storage` impl included yet) |
| `all-cloud` | no | Shorthand to enable `s3 + gcs + azure` together |

## License

Apache-2.0
