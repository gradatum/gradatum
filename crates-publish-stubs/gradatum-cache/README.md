# gradatum-cache

> Moka LRU in-process cache with checksum validation on hit. Implements D-perf-2 / B22 spec §6.1.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Public API

### Structs

```rust
/// Cache key: (vault_id, content_hash).
pub struct CacheKey {
    pub vault_id: String,
    pub content_hash: ContentHash,
}

/// Configuration for EffectiveNoteCache.
pub struct EffectiveNoteCacheConfig {
    pub max_capacity: u64,   // max entries (default: 1000)
    pub ttl_secs: u64,       // entry TTL (default: 300)
}

/// Moka-backed cache for EffectiveNote with checksum validation.
///
/// On cache hit: caller provides an async validator returning the current hash
/// from SQLite. If match → returns cached value. If mismatch → invalidates + returns None.
pub struct EffectiveNoteCache { ... }

impl EffectiveNoteCache {
    pub fn new(config: EffectiveNoteCacheConfig) -> Self

    /// Get a cached note, validating freshness via the provided async validator.
    pub async fn get<F, Fut>(
        &self,
        key: &CacheKey,
        validator: F,
    ) -> Option<Arc<EffectiveNote>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Option<ContentHash>>,

    pub fn insert(&self, key: CacheKey, note: Arc<EffectiveNote>, hash: ContentHash)

    pub fn invalidate(&self, key: &CacheKey)
}
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0