//! `impl VectorStore for SqliteIndex` — carve additif Étape 0.1.
//!
//! Toutes les méthodes délèguent aux méthodes concrètes `*_inner` de `SqliteIndex`
//! (renommées depuis `search_semantic`, `insert_note_embedding`, `get_note_embedding`
//! pour éviter la collision de nom avec les méthodes de trait).
//!
//! ## Contention
//!
//! Les 3 traits partagent un `Arc<Mutex<Connection>>` unique en v0.3.0.

use async_trait::async_trait;

use gradatum_core::{error::GradatumError, identity::NoteId, VectorStore};

use crate::SqliteIndex;

#[async_trait]
impl VectorStore for SqliteIndex {
    /// Insère ou met à jour un embedding — délègue à `insert_note_embedding_inner`.
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

    /// Relit un vecteur d'embedding — délègue à `get_note_embedding_inner`.
    async fn get_note_embedding(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
    ) -> Result<Option<Vec<f32>>, GradatumError> {
        self.get_note_embedding_inner(note_id, embedder_id).await
    }

    /// Recherche sémantique par cosine similarity — délègue à `search_semantic_inner`.
    async fn search_semantic(
        &self,
        vault_id: &str,
        embedder_id: &str,
        query_emb: &[f32],
        limit: usize,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError> {
        self.search_semantic_inner(vault_id, embedder_id, query_emb, limit)
            .await
    }
}
