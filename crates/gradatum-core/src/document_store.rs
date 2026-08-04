//! CRUD storage contract for notes (documents).
//!
//! [`DocumentStore`] is the minimal trait for reading and writing notes in an index.
//! It is a sub-trait of the legacy [`Index`](crate::index::Index) and is designed to be
//! consumed by crates that only need document operations
//! (`gradatum-vault`, `gradatum-context`), without depending on `gradatum-index`.
//!
//! ## Planned evolution
//!
//! The `Note` type will converge to `Document` in a future stable release —
//! `write_note` will be renamed `write`.
//! `GradatumError` will converge to a dedicated `StoreError` (cf. `QueueStore`).

use async_trait::async_trait;

use crate::error::GradatumError;
use crate::identity::{ContentHash, NoteId};
use crate::index::NoteRecord;
use crate::note::Note;
use crate::scope::{AclCheckedVaultId, VaultId};
use crate::status::NoteStatus;

/// CRUD storage contract for notes — async, thread-safe.
///
/// Implemented by `gradatum-index::SqliteIndex`.
/// Consumed by `gradatum-vault` without depending on the concrete SQLite implementation.
/// (An earlier revision named a second consumer, `gradatum-context`; no such crate exists
/// in the workspace.)
///
/// ## Stability
///
/// `#[stability::unstable]` — the API may change before a stable release.
/// The `Note` type will converge to `Document` — `write_note` will be renamed `write`.
/// `GradatumError` will converge to a dedicated `StoreError` (cf. `QueueStore`).
///
/// ## Contention
///
/// The current implementation shares a single `Arc<Mutex<Connection>>` between the three
/// traits (`DocumentStore`, `IndexStore`, `VectorStore`). Implementations must ensure `MutexGuard`
/// does not cross `.await` points. Physical connection separation is planned for a later release.
#[async_trait]
pub trait DocumentStore: Send + Sync {
    /// Writes or updates a note in the index (idempotent upsert).
    ///
    /// `Note.id` is the primary ULID key. Concurrent writes with the same identifier
    /// must not corrupt state — implementations must be atomic.
    ///
    /// # Side effects
    ///
    /// Synchronously updates the FTS5 table (`notes_fts`) when the index supports it.
    ///
    /// # Future rename
    ///
    /// Will be renamed `write` when `Note` is renamed to `Document`.
    async fn write_note(&self, note: &Note) -> Result<(), GradatumError>;

    /// Returns the stored `ContentHash` for a note within a vault, if it exists.
    ///
    /// Returns `None` if the note has not yet been indexed in `vault_id`.
    ///
    /// ## Vault scoping
    ///
    /// `vault_id` scopes the lookup to a single tenant: with the composite key
    /// `(vault_id, id)`, a homonymous note (same ULID) in another vault must not be
    /// returned. This is the freshness source of the moka cache validator.
    async fn get_content_hash(
        &self,
        vault_id: &str,
        id: NoteId,
    ) -> Result<Option<ContentHash>, GradatumError>;

    /// Returns the full record for a note by its ULID.
    ///
    /// Returns `None` if the note does not exist or is a sentinel.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    async fn get_note(
        &self,
        tenant_id: &str,
        note_id_ulid: &str,
    ) -> Result<Option<NoteRecord>, GradatumError>;

    /// Lists notes in a vault filtered by status.
    ///
    /// Results sorted by `updated DESC NULLS LAST, created DESC`.
    async fn list_by_status(
        &self,
        vault_id: &VaultId,
        status: NoteStatus,
    ) -> Result<Vec<NoteId>, GradatumError>;

    // ── dyn-wiring methods ─────────────────────────────────────────────────────

    /// Deactivates a note by setting its status to `downgraded`.
    ///
    /// Updates `status`, `status_reason`, `replaced_by`, `status_changed`, `updated`.
    /// Idempotent if the note is already downgraded.
    ///
    /// ## Vault scoping
    ///
    /// `vault` is the ACL-Write-checked target vault ([`AclCheckedVaultId`]). The mutation
    /// filters `WHERE vault_id = <vault> AND id = <note_id>` on the vault-shared `notes`
    /// table: a note owned by another vault is invisible (`NoteNotFound`), closing the
    /// cross-vault mutation-by-ULID hole. At `multi_tenant.enabled = false` the vault is
    /// always `main`, so the added predicate is transparent (byte-identical).
    ///
    /// # Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent **from `vault`** (same 404
    ///   whether the note exists in another vault or not at all — no existence oracle).
    /// - `GradatumError::Storage` on SQLite error.
    async fn downgrade_note(
        &self,
        vault: &AclCheckedVaultId,
        note_id: &NoteId,
        reason: &str,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError>;

    /// Partial PATCH of a note's status (status, reason, replaced_by).
    ///
    /// Updates only the supplied fields (`None` = unchanged).
    /// `status_changed` is updated only when `status` is supplied.
    /// `updated` is always refreshed.
    ///
    /// ## Vault scoping
    ///
    /// `vault` is the ACL-Write-checked target vault: the mutation filters
    /// `WHERE vault_id = <vault> AND id = <note_id>` — see [`Self::downgrade_note`].
    ///
    /// # Errors
    ///
    /// - `GradatumError::NoteNotFound` if no note matches `note_id` **in `vault`**.
    /// - `GradatumError::Storage` on SQLite error.
    async fn patch_note_status(
        &self,
        vault: &AclCheckedVaultId,
        note_id: &NoteId,
        status: Option<&str>,
        status_reason: Option<&str>,
        replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError>;

    /// Moves a note to a new `locus` (`UPDATE notes.locus`).
    ///
    /// Index-level mutation (metadata/ACL): `locus` is not an FTS column,
    /// so no FTS re-index is required. The ULID is preserved (no redirect table).
    /// Consistent with `downgrade_note` / `patch_note_status`. Idempotent (no-op UPDATE
    /// when the value is unchanged). `new_locus` must already be validated via `LocusId::parse`.
    ///
    /// ## Vault scoping
    ///
    /// `vault` is the ACL-Write-checked target vault: the mutation filters
    /// `WHERE vault_id = <vault> AND id = <note_id>` — see [`Self::downgrade_note`].
    ///
    /// # Errors
    /// - `GradatumError::NoteNotFound` if no note matches `note_id` **in `vault`**.
    /// - `GradatumError::Storage` on SQLite error.
    async fn update_note_locus(
        &self,
        vault: &AclCheckedVaultId,
        note_id: &NoteId,
        new_locus: &crate::scope::LocusId,
    ) -> Result<(), GradatumError>;

    /// Updates the `title` column of an existing note.
    ///
    /// Idempotent. In production the curator logs failures without propagating.
    /// Via this trait, the result is propagated — the caller decides whether to ignore it.
    /// Returns the number of rows affected (`0` if no note matches `(vault_id, note_id)`).
    ///
    /// ## Vault scoping (C4-1e, A2)
    ///
    /// The mutation filters `WHERE vault_id = <vault_id> AND id = <note_id>`: a write
    /// targeting one vault never touches a homonymous note in another vault.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn upsert_note_title(
        &self,
        vault_id: &str,
        note_id: &NoteId,
        title: &str,
    ) -> Result<usize, GradatumError>;

    // ── Semantic Forget ───────────────────────────────────────────────────────

    /// Marks a note as forgotten in the index.
    ///
    /// Sets `forgotten=1`, `forgotten_at=<now_ms>`, `forgotten_by=<by>`.
    /// Idempotent: a second call updates `forgotten_at` and `forgotten_by`.
    ///
    /// ## Boundary
    ///
    /// Operates on the SQLite index only — does NOT synchronise the YAML frontmatter.
    /// Frontmatter synchronisation is performed via `Vault::write_note_with_id`.
    ///
    /// # Errors
    ///
    /// - `GradatumError::NoteNotFound` if no note matches `note_id`.
    /// - `GradatumError::Storage` if the SQLite query fails.
    async fn mark_forgotten(
        &self,
        vault_id: &str,
        note_id: &str,
        by: Option<&str>,
    ) -> Result<(), GradatumError>;

    /// Re-asserts the forgotten mark WITHOUT rewriting the audit columns.
    ///
    /// Sets `forgotten=1` and leaves `forgotten_at` / `forgotten_by` untouched, `NULL`
    /// included. Repairs an index that reports a note as live while the vault frontmatter
    /// reports it as forgotten — a divergence produced by two ordinary paths, not by a
    /// race: `vault_unforgot` clears the index mark without touching the `.md`, and the
    /// internal `persist/forget` endpoint answers `200` best-effort when its
    /// `mark_forgotten` step fails after the vault write succeeded.
    ///
    /// Distinct from [`Self::mark_forgotten`], which stamps `forgotten_at`/`forgotten_by`
    /// unconditionally: replaying that one on an already-forgotten note destroys the audit
    /// trail of the FIRST forget. Repairing the index must cost nothing in auditability.
    ///
    /// ## Boundary
    ///
    /// Index only, like [`Self::mark_forgotten`] — never the YAML frontmatter.
    ///
    /// ## Idempotence
    ///
    /// Fully idempotent: every column, `forgotten` included, is left unchanged on replay.
    ///
    /// # Errors
    ///
    /// - `GradatumError::NoteNotFound` if no note matches `note_id`.
    /// - `GradatumError::Storage` if the SQLite query fails.
    async fn reassert_forgotten(&self, vault_id: &str, note_id: &str) -> Result<(), GradatumError>;

    /// Clears the forgotten mark on a note.
    ///
    /// Resets `forgotten=0`, `forgotten_at=NULL`, `forgotten_by=NULL`.
    /// Idempotent: a note that is already not forgotten remains unchanged.
    ///
    /// # Errors
    ///
    /// - `GradatumError::NoteNotFound` if no note matches `note_id`.
    /// - `GradatumError::Storage` if the SQLite query fails.
    async fn unmark_forgotten(&self, vault_id: &str, note_id: &str) -> Result<(), GradatumError>;

    /// Lists notes with `forgotten=1`, sorted by `forgotten_at DESC`.
    ///
    /// Cursor-based pagination: `cursor` = last received `note_id` (exclusive).
    /// `limit` is capped at 500 by the implementation.
    ///
    /// ## Return value
    ///
    /// `Vec<(id, title, section, forgotten_at_ms, forgotten_by)>`:
    /// - `id`: ULID string.
    /// - `title`: H1 title (may be `None`).
    /// - `section`: section name (e.g. `"decisions"`).
    /// - `forgotten_at_ms`: epoch-millisecond timestamp.
    /// - `forgotten_by`: optional actor identifier.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn list_forgotten(
        &self,
        vault_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Vec<(String, Option<String>, String, i64, Option<String>)>, GradatumError>;

    /// Counts the total number of forgotten notes for a vault.
    ///
    /// Used by `GET /api/v1/vault/forgotten` for the `total` field
    /// (global count, independent of pagination).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn count_forgotten(&self, vault_id: &str) -> Result<usize, GradatumError>;

    /// Counts notes grouped by status (`GROUP BY status`).
    ///
    /// **Out-of-enum tolerant**: the key is the raw SQL status string (e.g.
    /// `"live"`, `"pending-review"`, or `"downgraded"` from legacy rows not present
    /// in `NoteStatus`). The caller places unknown values in a `legacy` bucket —
    /// no data loss, no panic. Sentinel notes are excluded.
    ///
    /// # Errors
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn count_notes_by_status(
        &self,
        vault_id: &str,
    ) -> Result<std::collections::HashMap<String, u64>, GradatumError>;
}
