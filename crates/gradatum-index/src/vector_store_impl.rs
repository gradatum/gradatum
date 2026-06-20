//! `impl VectorStore for SqliteIndex`.
//!
//! All methods delegate to the `*_inner` concrete methods on `SqliteIndex`
//! (renamed from `search_semantic`, `insert_note_embedding`, `get_note_embedding`
//! to avoid name collision with the trait methods).
//!
//! ## Contention
//!
//! All three traits share a single `Arc<Mutex<Connection>>` (v0.3.0 design).
//!
//! ## Routage ANN (v0.5.3 ANN-5)
//!
//! `search_semantic` route selon `ann_enabled` :
//! - `true`  → `search_ann_inner` (vec0 sqlite-vec, ef_search configurable).
//! - `false` → `search_semantic_inner` (brute-force cosine, comportement historique).
//!
//! Fallback de sûreté : si `search_ann_inner` retourne une erreur (extension absente,
//! "no such module: vec0"), on logue un warn et on bascule automatiquement sur le
//! chemin brute-force. Aucun panic, aucun retour de 0 résultats sur erreur ANN seule.

use async_trait::async_trait;

use gradatum_core::{VectorStore, error::GradatumError, identity::NoteId};

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

    /// Semantic search — route vers ANN (sqlite-vec) ou brute-force cosine selon config.
    ///
    /// ## Routage (v0.5.3 ANN-5)
    ///
    /// Si `ann_is_enabled()` :
    /// 1. Tente `search_ann_inner` (vec0 sqlite-vec, ef_search = `ann_ef_search()`).
    /// 2. En cas d'erreur ANN (extension absente, "no such module: vec0", table manquante) :
    ///    - Logue `tracing::warn!` avec le message d'erreur.
    ///    - Bascule sur `search_semantic_inner` (brute-force) — pas de panic.
    ///
    /// Si `ann_is_enabled()` vaut `false` (défaut) :
    /// → `search_semantic_inner` directement (comportement byte-compat antérieur).
    async fn search_semantic(
        &self,
        vault_id: &str,
        embedder_id: &str,
        query_emb: &[f32],
        limit: usize,
        locus: Option<&str>,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError> {
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
