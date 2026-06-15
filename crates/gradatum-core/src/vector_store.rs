//! Vector storage and semantic search contract.
//!
//! [`VectorStore`] exposes embedding and semantic search operations (cosine
//! similarity). Designed to be consumed by search pipelines without depending
//! on `gradatum-index`.
//!
//! ## Future evolution
//!
//! The names `insert_note_embedding` / `search_semantic` may converge toward
//! `upsert` / `search` with `VectorMeta` / `VectorMatch` when migrating to ANN
//! (sqlite-vec). `GradatumError` may converge toward a dedicated `StoreError`.

use async_trait::async_trait;

use crate::error::GradatumError;
use crate::identity::NoteId;

/// Async, thread-safe vector storage and semantic search contract.
///
/// Implemented by `gradatum-index::SqliteIndex` (raw cosine over f32 BLOBs).
/// New implementations can substitute ANN backends (sqlite-vec, hnswlib) without
/// modifying consumers.
///
/// ## Stability
///
/// This trait's API may change before v1.0.0. Method names
/// `insert_note_embedding`/`search_semantic` may converge toward `upsert`/`search`
/// with `VectorMeta`/`VectorMatch` when switching to ANN.
///
/// ## Connection contention
///
/// Currently shares a single `Arc<Mutex<Connection>>` with `DocumentStore` and
/// `IndexStore`. Physical connection separation is planned for a future release.
// `#[stability::unstable]` deferred — requires `[features] unstable-storage-traits = []`
// in gradatum-core/Cargo.toml + opt-in from all workspace consumers.
// The stability macro does not prevent compilation (no E0365); without the declared
// feature it would emit a deprecated warning on every consumer.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Inserts or updates the embedding for a note.
    ///
    /// The vector is serialised as an f32 little-endian BLOB (4 bytes/dimension).
    /// `ON CONFLICT(note_id, embedder_id)` replaces the existing vector.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if `vector.len() != dim` or if the write fails.
    async fn insert_note_embedding(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
        dim: u16,
        vector: &[f32],
    ) -> Result<(), GradatumError>;

    /// Reads an existing embedding vector from the index.
    ///
    /// Returns `None` if no embedding exists for `(note_id, embedder_id)`.
    /// Returns `Some(Vec<f32>)` after decoding the f32 little-endian BLOB.
    ///
    /// Used by the embedding pipeline to avoid recomputing an already-stored
    /// embedding when `computed_at` is recent.
    async fn get_note_embedding(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
    ) -> Result<Option<Vec<f32>>, GradatumError>;

    /// Semantic search by cosine similarity within a vault.
    ///
    /// Loads all embeddings for `embedder_id` in the vault, computes cosine
    /// similarity against `query_emb`, and returns `NoteId`s sorted by descending
    /// score (up to `limit` results).
    ///
    /// Notes with `status = 'downgraded'` and sentinels (`__sentinel__*`) are excluded.
    ///
    /// Returns an empty `Vec` if `query_emb` is the zero vector (norm = 0).
    ///
    /// ## Optional locus filter
    ///
    /// When `locus` is `Some(prefix)`, only notes whose `locus` starts with `prefix`
    /// are returned. The prefix is automatically escaped against SQLite LIKE
    /// metacharacters in the implementation.
    ///
    /// ## ANN migration
    ///
    /// Will converge toward ANN (sqlite-vec) for corpora larger than ~10k notes.
    async fn search_semantic(
        &self,
        vault_id: &str,
        embedder_id: &str,
        query_emb: &[f32],
        limit: usize,
        locus: Option<&str>,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError>;
}
