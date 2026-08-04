//! `EffectiveNote` cache with checksum validation on hit.
//!
//! ## Design
//!
//! - Composite key `(VaultId, NoteId, u64)`: `VaultId` partitions the key per vault
//!   (fail-safe — a `NoteId` is only unique within a vault, so the key MUST carry the
//!   vault to avoid cross-vault collisions if a cache is ever shared); `NoteId` identifies
//!   the note; `u64` is the hash of the `OverrideScope` computed by the caller (avoids
//!   importing the full type into the key).
//! - Stored value: `Entry { value: Arc<EffectiveNote>, content_hash: ContentHash }`.
//! - On **cache hit**: the caller provides an async `validator` closure that returns the
//!   current hash from SQLite. Match → returns cached value. Mismatch → invalidates entry + returns `None`.
//! - On **cache miss**: `validator` is not called (zero overhead on miss).
//!
//! ## Defaults
//!
//! | Parameter      | Value  |
//! |---|---|
//! | `max_capacity` | 10 000 |
//! | `time_to_live` | 5 min  |
//! | `time_to_idle` | 60 s   |
//!
//! ## Cost
//!
//! +200 µs p99 per read (SQLite validator call on hit) — acceptable trade-off against the
//! risk of stale reads under concurrent worker writes and server reads.

use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;

use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::note::EffectiveNote;
use gradatum_core::scope::VaultId;

/// Composite cache key: `(vault_id, note_id, scope_hash)`.
///
/// - `vault_id` partitions entries per vault. A `NoteId` is only unique *within* a
///   vault, so the vault MUST be part of the key: without it, two vaults sharing a
///   cache would collide on the same `(note_id, scope_hash)` and leak entries across
///   vaults (C4 — fail-safe, closed structurally before any cache sharing).
/// - `scope_hash` is a `u64` computed by the caller from an `OverrideScope`
///   (e.g. via `std::hash::Hasher`). This design avoids importing `OverrideScope`
///   into the cache key and keeps the key size small.
pub type CacheKey = (VaultId, NoteId, u64);

/// Configuration for `EffectiveNoteCache`.
#[derive(Debug, Clone)]
pub struct EffectiveNoteCacheConfig {
    /// Maximum number of entries in the cache (approximate LRU eviction).
    pub max_capacity: u64,
    /// Maximum time-to-live for an entry since its insertion.
    pub time_to_live: Duration,
    /// Maximum idle time before an entry expires.
    pub time_to_idle: Duration,
}

impl Default for EffectiveNoteCacheConfig {
    fn default() -> Self {
        Self {
            max_capacity: 10_000,
            time_to_live: Duration::from_secs(300),
            time_to_idle: Duration::from_secs(60),
        }
    }
}

/// Internal cache entry: stored value paired with its checksum hash.
#[derive(Clone)]
struct Entry {
    value: Arc<EffectiveNote>,
    content_hash: ContentHash,
}

/// Moka LRU cache for `EffectiveNote` with checksum validation on hit.
///
/// Thread-safe. Share it across Axum handlers via `Arc<EffectiveNoteCache>`.
///
/// The type does **not** implement `Clone` — only the inner moka `future::Cache` is an
/// `Arc` wrapper, and that is not re-exposed. `Arc` is the only sharing route.
pub struct EffectiveNoteCache {
    inner: Cache<CacheKey, Entry>,
}

impl EffectiveNoteCache {
    /// Builds a new cache with the provided configuration.
    pub fn new(cfg: EffectiveNoteCacheConfig) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(cfg.max_capacity)
                .time_to_live(cfg.time_to_live)
                .time_to_idle(cfg.time_to_idle)
                .build(),
        }
    }

    /// Inserts or replaces an entry in the cache.
    ///
    /// The call is async because moka notifies eviction listeners asynchronously.
    pub async fn insert(
        &self,
        key: CacheKey,
        value: Arc<EffectiveNote>,
        content_hash: ContentHash,
    ) {
        self.inner
            .insert(
                key,
                Entry {
                    value,
                    content_hash,
                },
            )
            .await;
    }

    /// Returns the cached value if the hash is still valid, otherwise `None`.
    ///
    /// ## Behaviour
    ///
    /// 1. **Cache miss**: returns `Ok(None)` without calling `validator`.
    /// 2. **Cache hit**: calls `validator(note_id)` to fetch the current hash from SQLite.
    ///    - Matching hash → returns `Ok(Some(arc_value))`.
    ///    - Differing hash → invalidates the entry and returns `Ok(None)`.
    ///    - `validator` returns an error → propagates `Err(e)` without invalidating the cache
    ///      (the entry is not confirmed stale; the error is treated as a transient DB error).
    ///
    /// ## Generic parameters
    ///
    /// - `F`: closure that takes a `NoteId` and returns a `Future<Output = Result<ContentHash, E>>`.
    /// - `E`: error type propagated as-is (e.g. `sqlx::Error`, `rusqlite::Error`).
    ///
    /// ## Example
    ///
    /// ```ignore
    /// let result = cache.get(key, |note_id| async move {
    ///     db_store.fetch_content_hash(note_id).await
    /// }).await?;
    /// ```
    pub async fn get<F, Fut, E>(
        &self,
        key: CacheKey,
        validator: F,
    ) -> Result<Option<Arc<EffectiveNote>>, E>
    where
        F: FnOnce(NoteId) -> Fut,
        Fut: std::future::Future<Output = Result<ContentHash, E>>,
    {
        let Some(entry) = self.inner.get(&key).await else {
            // Cache miss — validator not called (zero overhead).
            return Ok(None);
        };

        // key.1 = NoteId (key.0 = VaultId partition). Le validator reçoit le NoteId.
        let live_hash = validator(key.1).await?;

        if live_hash == entry.content_hash {
            return Ok(Some(entry.value));
        }

        // Hash mismatch: stale entry — invalidate immediately.
        self.inner.invalidate(&key).await;
        Ok(None)
    }

    /// Explicitly invalidates a cache entry.
    ///
    /// Call after writing a note to prevent stale reads.
    pub async fn invalidate(&self, key: &CacheKey) {
        self.inner.invalidate(key).await;
    }

    /// Returns the current number of entries in the cache (approximate, moka best-effort).
    ///
    /// Useful for metrics and tests. Do not use for business logic.
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }

    /// Drives pending eviction tasks (TTL/TTI/LRU) to completion.
    ///
    /// Useful in tests to observe eviction after `tokio::time::sleep`.
    /// In production, moka runs these tasks automatically in the background.
    pub async fn run_pending_tasks(&self) {
        self.inner.run_pending_tasks().await;
    }
}
