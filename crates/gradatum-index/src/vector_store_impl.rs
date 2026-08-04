//! `impl VectorStore for SqliteIndex`.
//!
//! All methods delegate to the `*_inner` concrete methods on `SqliteIndex`
//! (renamed from `search_semantic`, `insert_note_embedding`, `get_note_embedding`
//! to avoid name collision with the trait methods).
//!
//! ## Contention
//!
//! All three traits share a single `Arc<Mutex<Connection>>`.
//!
//! ## ANN routing
//!
//! `search_semantic` routes on `ann_enabled`:
//! - `true`  → `search_ann_inner` (sqlite-vec vec0, configurable `ef_search`).
//! - `false` → `search_semantic_inner` (brute-force cosine).
//!
//! Safety fallback: if `search_ann_inner` returns an error (extension absent,
//! "no such module: vec0"), the failure is logged at `warn` level and the brute-force path
//! takes over automatically. An ANN error alone never panics and never yields zero
//! results.

use async_trait::async_trait;

use gradatum_core::{VectorStore, error::GradatumError, identity::NoteId};

use crate::SqliteIndex;

#[async_trait]
impl VectorStore for SqliteIndex {
    /// Inserts or updates an embedding — delegates to `insert_note_embedding_inner`.
    async fn insert_note_embedding(
        &self,
        vault_id: &str,
        note_id: &NoteId,
        embedder_id: &str,
        dim: u16,
        vector: &[f32],
    ) -> Result<(), GradatumError> {
        self.insert_note_embedding_inner(vault_id, note_id, embedder_id, dim, vector)
            .await
    }

    /// Reads back an embedding vector — delegates to `get_note_embedding_inner`.
    async fn get_note_embedding(
        &self,
        vault_id: &str,
        note_id: &NoteId,
        embedder_id: &str,
    ) -> Result<Option<Vec<f32>>, GradatumError> {
        self.get_note_embedding_inner(vault_id, note_id, embedder_id)
            .await
    }

    /// Semantic search — routes to ANN (sqlite-vec) or brute-force cosine, per configuration.
    ///
    /// ## Routing
    ///
    /// When `ann_is_enabled()` is true:
    /// 1. `search_ann_inner` is tried first (sqlite-vec vec0, `ef_search = ann_ef_search()`).
    /// 2. On an ANN error (extension absent, "no such module: vec0", missing table) the
    ///    error is logged through `tracing::warn!` and the call falls back to
    ///    `search_semantic_inner` — it never panics.
    ///
    /// When `ann_is_enabled()` is false (the default), `search_semantic_inner` is called
    /// directly.
    async fn search_semantic(
        &self,
        vault_id: &gradatum_core::scope::AclCheckedVaultId,
        embedder_id: &str,
        query_emb: &[f32],
        limit: usize,
        locus: Option<&str>,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError> {
        let vault_id = vault_id.as_str();
        if self.ann_is_enabled() {
            let ef_search = self.ann_ef_search();
            match crate::sqlite_vec::search_ann_inner(
                &self.conn,
                vault_id,
                embedder_id,
                query_emb,
                limit,
                ef_search,
                locus,
            )
            .await
            {
                Ok(results) => return Ok(results),
                Err(e) => {
                    // Fallback de sûreté : ANN indisponible (extension absente, table manquante).
                    // On ne panic PAS — on dégrade gracieusement vers brute-force.
                    tracing::warn!(
                        vault_id = %vault_id,
                        embedder_id = %embedder_id,
                        error = %e,
                        "search_semantic: ANN sqlite-vec failed — fallback brute-force cosine"
                    );
                    // Chute vers brute-force ci-dessous.
                }
            }
        }

        // Chemin brute-force (défaut ou fallback ANN).
        self.search_semantic_inner(vault_id, embedder_id, query_emb, limit, locus)
            .await
    }
}
