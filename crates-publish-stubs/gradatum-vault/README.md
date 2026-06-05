# gradatum-vault

> Multi-vault registry, lifecycle management (create/list/swap/delete), note write pipeline, and drift orchestration.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// Top-level vault handle — registry + lifecycle operations.
pub struct Vault { ... }

impl Vault {
    /// Create a new vault at `root` with `tenant_id`.
    pub fn create(root: &Path, tenant_id: &str) -> Result<Self, VaultError>

    /// Open an existing vault at `root`.
    pub fn open(root: &Path) -> Result<Self, VaultError>

    /// Write a note to the vault (ContentHash + persist .md + upsert index).
    pub async fn write_note(&self, note: Note) -> Result<NoteId, VaultError>

    /// Read a note by ID from the vault.
    pub async fn read_note(&self, id: &NoteId) -> Result<Note, VaultError>

    /// Trigger Phase A drift check.
    pub async fn drift_check(&self) -> Result<Vec<NoteId>, VaultError>
}

/// Metadata override — applies on top of base frontmatter at read time.
pub struct NoteMetadataOverride { ... }

/// Immutable history entry for a note (scaffold Phase 1).
pub struct NoteHistoryEntry {
    pub note_id: NoteId,
    pub timestamp: DateTime<Utc>,
    pub content_hash: ContentHash,
    pub author: Option<AuthorId>,
}
```

### Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    Storage(StorageError),
    Index(IndexError),
    Markdown(MarkdownError),
    Core(GradatumError),
    NoteNotFound(NoteId),
    VaultAlreadyExists(PathBuf),
    Io(std::io::Error),
}
```

## Architecture (L2)

```
Vault (L2)
├── gradatum-core    (L0) — primitives, traits, errors
├── gradatum-markdown (L1) — parse/write .md
├── gradatum-cache   (L1) — EffectiveNoteCache (moka)
├── gradatum-index   (L1) — SqliteIndex
└── gradatum-storage (L1) — FileStorage (OpenDAL)
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0