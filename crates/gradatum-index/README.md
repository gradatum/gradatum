# gradatum-index

> SQLite + FTS5 index layer — implements `DocumentStore`, `IndexStore`, and `VectorStore` traits with three-level drift detection.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-index` provides `SqliteIndex`, the primary storage implementation for gradatum.
It combines full-text search via SQLite FTS5 with dense vector storage for semantic search,
and exposes the `Index` blanket trait through implementations of all three `gradatum-core`
storage traits.

Key features:

- **FTS5 full-text search** with BM25 ranking over note bodies and metadata.
- **Cosine vector similarity** computed directly in SQLite (no external vector database).
- **Four mandatory PRAGMAs** on open: `WAL`, `synchronous=NORMAL`, `busy_timeout=5000`,
  `foreign_keys=ON`.
- **Schema migrations** — applied automatically from embedded SQL migration files.
- **Three-level drift detection** via `drift::scan_phase_a`:
  - Level 1: file size check (fast, no read).
  - Level 2: first 4 KB prefix hash.
  - Level 3: full SHA-256 (only when Level 2 mismatches).

## Usage

```toml
[dependencies]
gradatum-index = "0.4.0"
```

```rust
use gradatum_index::SqliteIndex;

let index = SqliteIndex::open(Path::new("/var/lib/gradatum/index.db")).await?;
// or for tests:
let index = SqliteIndex::open_in_memory().await?;
```

## License

Apache-2.0
