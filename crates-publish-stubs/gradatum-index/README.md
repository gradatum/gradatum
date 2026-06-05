# gradatum-index

> SQLite + FTS5 index layer with three-level drift detection (Phase A).

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// SQLite + FTS5 implementation of gradatum_core::index::Index.
/// Applies 4 mandatory PRAGMAs on open: WAL, synchronous=NORMAL,
/// busy_timeout=5000, foreign_keys=ON.
pub struct SqliteIndex { ... }

impl SqliteIndex {
    /// Open (or create) an index at the given SQLite file path.
    pub async fn open(path: &Path) -> Result<Self, IndexError>

    /// Open an in-memory index (tests / ephemeral use).
    pub async fn open_in_memory() -> Result<Self, IndexError>
}
```

### Drift detection

```rust
/// Three-level drift scan (Phase A).
///
/// Level 1 — file size check (fast, no I/O).
/// Level 2 — first 4KB prefix hash.
/// Level 3 — full SHA-256 (only when Level 2 mismatch).
///
/// Returns the list of note IDs whose on-disk content diverges from index.
pub async fn scan_phase_a(
    index: &SqliteIndex,
    storage: &dyn Storage,
) -> Result<Vec<NoteId>, IndexError>
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0