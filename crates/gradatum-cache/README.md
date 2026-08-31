# gradatum-cache

> Moka LRU in-process cache with checksum validation on hit.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-cache` provides `EffectiveNoteCache`, a Moka-backed async LRU cache for
deserialized notes. It implements a validated read-through pattern:

- On cache **hit**: the caller provides an async validator that fetches the current
  `ContentHash` from SQLite. If it matches the cached hash, the cached value is returned.
  If it does not match, the entry is invalidated and `None` is returned.
- On cache **miss**: returns `None`; the caller populates from disk and inserts.

This approach avoids stale reads without requiring write-through coordination.

## Usage

```toml
[dependencies]
gradatum-cache = "2.1.0"
```

```rust
use gradatum_cache::{EffectiveNoteCache, EffectiveNoteCacheConfig};

let cache = EffectiveNoteCache::new(EffectiveNoteCacheConfig {
    max_capacity: 1_000,
    time_to_live: Duration::from_secs(300),
    time_to_idle: Duration::from_secs(60),
});

let note = cache.get(key, |note_id| async move {
    // return current ContentHash from SQLite
    db.fetch_hash(note_id).await
}).await?;
```

## License

Apache-2.0
