//! `impl DocumentStore for SqliteIndex`.
//!
//! All methods delegate to inherent methods on `SqliteIndex`
//! (defined in `sqlite.rs`).
//!
//! Exposes: `write_note`, `get_content_hash`, `get_note`, `list_by_status`,
//! `downgrade_note`, `patch_note_status`, `mark_forgotten`, `unmark_forgotten`, `list_forgotten`.
//!
//! ## Contention
//!
//! All three traits (`DocumentStore`, `IndexStore`, `VectorStore`) share a single
//! `Arc<Mutex<Connection>>` (v0.3.0 design). Physical separation was introduced in v0.4.0.

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
    /// Writes or updates a note — delegates to the `upsert_note` inherent method.
    ///
    /// `upsert_note` is an inherent `pub(crate)` method in `sqlite.rs`.
    async fn write_note(&self, note: &Note) -> Result<(), GradatumError> {
        self.upsert_note(note).await
    }

    /// Returns the content hash — delegates to the `get_content_hash` inherent method.
    async fn get_content_hash(&self, id: NoteId) -> Result<Option<ContentHash>, GradatumError> {
        self.get_content_hash(id).await
    }

    /// Returns the full note record — delegates to `get_note_inner` (concrete method in `queries.rs`).
    async fn get_note(
        &self,
        tenant_id: &str,
        note_id_ulid: &str,
    ) -> Result<Option<NoteRecord>, GradatumError> {
        self.get_note_inner(tenant_id, note_id_ulid).await
    }

    /// Lists notes by status — delegates to the `list_by_status` inherent method.
    async fn list_by_status(
        &self,
        vault_id: &VaultId,
        status: NoteStatus,
    ) -> Result<Vec<NoteId>, GradatumError> {
        self.list_by_status(vault_id, status).await
    }

    // ── Promotions Étape 0.2a ─────────────────────────────────────────────────

    /// Downgrades a note — delegates to `SqliteIndex::downgrade_note`.
    async fn downgrade_note(
        &self,
        note_id: &NoteId,
        reason: &str,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        self.downgrade_note(note_id, reason, replaced_by).await
    }

    /// Partial status PATCH — delegates to `SqliteIndex::patch_note_status`.
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

    /// Upserts a note title — delegates to `SqliteIndex::upsert_note_title`.
    async fn upsert_note_title(&self, note_id: &NoteId, title: &str) -> Result<(), GradatumError> {
        self.upsert_note_title(note_id, title).await
    }

    /// Moves the locus of a note — delegates to `SqliteIndex::update_note_locus`.
    async fn update_note_locus(
        &self,
        note_id: &NoteId,
        new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), GradatumError> {
        self.update_note_locus(note_id, new_locus).await
    }

    // ── Semantic Forget ───────────────────────────────────────────────────────

    /// Marks a note as forgotten — delegates to `SqliteIndex::mark_forgotten`.
    async fn mark_forgotten(
        &self,
        vault_id: &str,
        note_id: &str,
        by: Option<&str>,
    ) -> Result<(), GradatumError> {
        self.mark_forgotten(vault_id, note_id, by).await
    }

    /// Clears the forgotten mark — delegates to `SqliteIndex::unmark_forgotten`.
    async fn unmark_forgotten(&self, vault_id: &str, note_id: &str) -> Result<(), GradatumError> {
        self.unmark_forgotten(vault_id, note_id).await
    }

    /// Lists paginated forgotten notes — delegates to `SqliteIndex::list_forgotten_notes`.
    async fn list_forgotten(
        &self,
        vault_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Vec<(String, Option<String>, String, i64, Option<String>)>, GradatumError> {
        self.list_forgotten_notes(vault_id, limit, cursor).await
    }

    /// Counts forgotten notes globally — delegates to `SqliteIndex::count_forgotten_notes`.
    ///
    /// Used by `GET /vault/forgotten` for the global `total` field,
    /// distinct from the current page size.
    async fn count_forgotten(&self, vault_id: &str) -> Result<usize, GradatumError> {
        self.count_forgotten_notes(vault_id).await
    }

    /// Counts notes by status — delegates to `SqliteIndex::count_notes_by_status`.
    async fn count_notes_by_status(
        &self,
        vault_id: &str,
    ) -> Result<std::collections::HashMap<String, u64>, GradatumError> {
        self.count_notes_by_status(vault_id).await
    }
}
