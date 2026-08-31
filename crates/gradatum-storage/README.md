# gradatum-storage

> Storage trait abstraction with an OpenDAL-backed filesystem or S3-compatible object backend.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-storage` defines the `Storage` async trait and provides a generic OpenDAL-backed
implementation, selected by configuration: `fs` (local filesystem, default) or `s3` (any
S3-compatible provider — AWS, OVH, MinIO, Ceph, Scaleway). The `gcs` and `azure` Cargo
features enable their respective OpenDAL client dependencies, but `build_storage` has no
match arm for either yet — the feature is compiled in, the backend is not selectable.


## Usage

```toml
[dependencies]
gradatum-storage = "2.1.0"
```

```rust
use gradatum_storage::FileStorage;
use gradatum_storage::Storage; // `read` is a trait method
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
| `fs` | yes | Filesystem backend via OpenDAL — selectable via `service = "fs"` |
| `s3` | no | S3-compatible object backend via OpenDAL — selectable via `service = "s3"` |
| `gcs` | no | Enables the OpenDAL GCS service dependency — not wired into `build_storage`, no selectable backend |
| `azure` | no | Enables the OpenDAL Azure Blob service dependency — not wired into `build_storage`, no selectable backend |
| `all-cloud` | no | Shorthand to enable `s3 + gcs + azure` together |

## License

Apache-2.0
