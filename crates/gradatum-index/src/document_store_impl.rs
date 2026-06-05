//! `impl DocumentStore for SqliteIndex` — carve additif Étape 0.1 + extension Étape 0.2a.
//!
//! Toutes les méthodes délèguent aux méthodes inhérentes de `SqliteIndex`
//! (définies dans `sqlite.rs`, anciennement dans `impl Index for SqliteIndex`).
//!
//! Étape 0.2a ajoute `downgrade_note` et `patch_note_status` (opérations de cycle de vie
//! appelées par les handlers — requises pour le flip `AppState.search: Arc<dyn Index>`).
//!
//! ## Contention
//!
//! Les 3 traits (`DocumentStore`, `IndexStore`, `VectorStore`) partagent
//! un `Arc<Mutex<Connection>>` unique en v0.3.0. Séparation physique = v0.4.0.

use async_trait::async_trait;

use gradatum_core::{
    error::GradatumError,
    identity::{ContentHash, NoteId},
    index::NoteRecord,
    note::Note,
    scope::VaultId,
    status::NoteStatus,
    DocumentStore,
};

use crate::SqliteIndex;

#[async_trait]
impl DocumentStore for SqliteIndex {
    /// Écrit ou met à jour une note — délègue à la méthode inhérente `upsert_note`.
    ///
    /// `upsert_note` était dans `impl Index for SqliteIndex` pré-Étape 0.1 —
    /// maintenant méthode inhérente `pub(crate)` dans `sqlite.rs`.
    async fn write_note(&self, note: &Note) -> Result<(), GradatumError> {
        self.upsert_note(note).await
    }

    /// Retourne le hash de contenu — délègue à la méthode inhérente `get_content_hash`.
    async fn get_content_hash(&self, id: NoteId) -> Result<Option<ContentHash>, GradatumError> {
        self.get_content_hash(id).await
    }

    /// Retourne le record complet — délègue à `get_note_inner` (méthode concrète queries.rs).
    async fn get_note(
        &self,
        tenant_id: &str,
        note_id_ulid: &str,
    ) -> Result<Option<NoteRecord>, GradatumError> {
        self.get_note_inner(tenant_id, note_id_ulid).await
    }

    /// Liste les notes par statut — délègue à la méthode inhérente `list_by_status`.
    async fn list_by_status(
        &self,
        vault_id: &VaultId,
        status: NoteStatus,
    ) -> Result<Vec<NoteId>, GradatumError> {
        self.list_by_status(vault_id, status).await
    }

    // ── Promotions Étape 0.2a ─────────────────────────────────────────────────

    /// Downgrade une note — délègue à `SqliteIndex::downgrade_note`.
    async fn downgrade_note(
        &self,
        note_id: &NoteId,
        reason: &str,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        self.downgrade_note(note_id, reason, replaced_by).await
    }

    /// PATCH partiel statut — délègue à `SqliteIndex::patch_note_status`.
    async fn patch_note_status(
        &self,
        note_id: &NoteId,
        status: Option<&str>,
        status_reason: Option<&str>,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        self.patch_note_status(note_id, status, status_reason, replaced_by)
            .await
    }

    /// Titre de note — délègue à `SqliteIndex::upsert_note_title`.
    async fn upsert_note_title(&self, note_id: &NoteId, title: &str) -> Result<(), GradatumError> {
        self.upsert_note_title(note_id, title).await
    }
}
