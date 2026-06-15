//! `impl VectorStore for SqliteIndex`.
//!
//! All methods delegate to the `*_inner` concrete methods on `SqliteIndex`
//! (renamed from `search_semantic`, `insert_note_embedding`, `get_note_embedding`
//! to avoid name collision with the trait methods).
//!
//! ## Contention
//!
//! All three traits share a single `Arc<Mutex<Connection>>` (v0.3.0 design).

use async_trait::async_trait;

use gradatum_core::{error::GradatumError, identity::NoteId, VectorStore};

use crate::SqliteIndex;

#[async_trait]
impl VectorStore for SqliteIndex {
    /// Inserts or updates an embedding — delegates to `insert_note_embedding_inner`.
    async fn insert_note_embedding(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
        dim: u16,
        vector: &[f32],
    ) -> Result<(), GradatumError> {
        self.insert_note_embedding_inner(note_id, embedder_id, dim, vector)
            .await
    }

    /// Reads back an embedding vector — delegates to `get_note_embedding_inner`.
    async fn get_note_embedding(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
    ) -> Result<Option<Vec<f32>>, GradatumError> {
        self.get_note_embedding_inner(note_id, embedder_id).await
    }

    /// Semantic search by cosine similarity — delegates to `search_semantic_inner`.
    async fn search_semantic(
        &self,
        vault_id: &str,
        embedder_id: &str,
        query_emb: &[f32],
        limit: usize,
        locus: Option<&str>,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError> {
        self.search_semantic_inner(vault_id, embedder_id, query_emb, limit, locus)
            .await
    }
}
