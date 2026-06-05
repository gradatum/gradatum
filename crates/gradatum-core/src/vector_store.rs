//! Contrat de stockage et recherche vectorielle.
//!
//! [`VectorStore`] expose les opérations d'embedding et de recherche sémantique
//! (cosine similarity en v0.3.0). Conçu pour être consommé par les futurs pipelines
//! de recherche sans dépendre de `gradatum-index`.
//!
//! ## Évolution v0.4.0
//!
//! Les noms `insert_note_embedding` / `search_semantic` convergeront vers `upsert` / `search`
//! avec `VectorMeta` / `VectorMatch` au moment du passage à ANN (sqlite-vec).
//! L'erreur `GradatumError` convergera vers `StoreError` dédié à v0.4.0.

use async_trait::async_trait;

use crate::error::GradatumError;
use crate::identity::NoteId;

/// Contrat de stockage et recherche vectorielle — async, thread-safe.
///
/// Implémenté par `gradatum-index::SqliteIndex` (Phase 1 : cosine brut sur BLOB).
/// Les nouvelles implémentations peuvent substituer ANN (sqlite-vec, hnswlib) sans
/// modifier les consommateurs.
///
/// ## Stabilité
///
/// `#[stability::unstable]` — l'API peut changer jusqu'à Silver (v1.0.0).
/// Backend cosine en v0.3.0 ; convergera vers ANN (sqlite-vec) à v0.4.0.
/// Les noms `insert_note_embedding`/`search_semantic` convergeront vers `upsert`/`search`
/// avec `VectorMeta`/`VectorMatch` au même moment.
///
/// ## Contention
///
/// En v0.3.0, ce trait partage un `Arc<Mutex<Connection>>` unique avec `DocumentStore`
/// et `IndexStore`. Séparation physique des connexions prévue à v0.4.0.
// AM1 : instabilité documentée ici et dans le module doc.
// `#[stability::unstable]` différé v0.4.0 — nécessite `[features] unstable-storage-traits = []`
// dans gradatum-core/Cargo.toml + opt-in de tous les consommateurs workspace.
// La macro stability n'empêche rien (pas d'E0365) ; sans la feature déclarée elle émettrait
// un deprecated warning sur chaque consommateur.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Insère ou met à jour l'embedding d'une note.
    ///
    /// Le vecteur est sérialisé en BLOB f32 little-endian (4 bytes/dim).
    /// `ON CONFLICT(note_id, embedder_id)` remplace le vecteur existant.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Storage` si `vector.len() != dim` ou si l'écriture échoue.
    async fn insert_note_embedding(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
        dim: u16,
        vector: &[f32],
    ) -> Result<(), GradatumError>;

    /// Relit un vecteur d'embedding depuis l'index.
    ///
    /// Retourne `None` si aucun embedding n'existe pour ce couple `(note_id, embedder_id)`.
    /// Retourne `Some(Vec<f32>)` après décodage BLOB f32 little-endian.
    ///
    /// Utilisé par le pipeline d'embed pour éviter un recalcul inutile
    /// (skip si embedding déjà présent et `computed_at` récent).
    async fn get_note_embedding(
        &self,
        note_id: &NoteId,
        embedder_id: &str,
    ) -> Result<Option<Vec<f32>>, GradatumError>;

    /// Recherche sémantique par cosine similarity dans un vault.
    ///
    /// Charge tous les embeddings du vault pour `embedder_id`, calcule le cosine
    /// avec `query_emb`, retourne les `NoteId` triés par score décroissant (limit `limit`).
    ///
    /// Notes avec `status = 'downgraded'` et sentinelles (`__sentinel__*`) sont exclues.
    ///
    /// Si `query_emb` est le vecteur nul (norme = 0), retourne un Vec vide.
    ///
    /// # Note v0.4.0
    ///
    /// Convergera vers ANN (sqlite-vec) pour des corpus > 10k notes.
    async fn search_semantic(
        &self,
        vault_id: &str,
        embedder_id: &str,
        query_emb: &[f32],
        limit: usize,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError>;
}
