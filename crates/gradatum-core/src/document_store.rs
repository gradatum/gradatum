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

    /// Returns the full records of several notes by ULID — the batched form of
    /// [`DocumentStore::get_note`].
    ///
    /// Records come back in the order of `note_ids`. A repeated identifier yields a
    /// **single** record, placed at the position of its first occurrence; an identifier
    /// the index does not know is omitted, so the result may be shorter than the request
    /// and callers must never assume a positional match.
    ///
    /// Every implementation — this default and the batched overrides alike — must answer
    /// exactly that.
    ///
    /// # Default implementation
    ///
    /// Loops over [`DocumentStore::get_note`], which is correct on every backend but costs
    /// one round-trip per distinct identifier. A backend able to batch natively (e.g. a
    /// single `WHERE id IN (…)` statement) should override it, so that a walk over a page
    /// of notes stays at a bounded number of reads — the same split as the queue store's
    /// `get_many`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if a query fails.
    async fn get_notes_many(
        &self,
        tenant_id: &str,
        note_ids: &[String],
    ) -> Result<Vec<NoteRecord>, GradatumError> {
        let mut records = Vec::with_capacity(note_ids.len());
        let mut seen = std::collections::HashSet::with_capacity(note_ids.len());
        for note_id in note_ids {
            if !seen.insert(note_id.as_str()) {
                continue;
            }
            if let Some(record) = self.get_note(tenant_id, note_id).await? {
                records.push(record);
            }
        }
        Ok(records)
    }

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

#[cfg(test)]
mod get_notes_many_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Store minimal : `get_note` est réel (et compté), le reste est inatteignable.
    ///
    /// Il ne surcharge **pas** `get_notes_many` : c'est l'implémentation par défaut du
    /// trait qui est sous test ici.
    struct MinimalNoteStore {
        rows: std::collections::HashMap<String, NoteRecord>,
        unit_reads: AtomicUsize,
    }

    impl MinimalNoteStore {
        fn with(ids: &[&str]) -> Self {
            let rows = ids
                .iter()
                .map(|id| (id.to_string(), record(id)))
                .collect::<std::collections::HashMap<_, _>>();
            Self {
                rows,
                unit_reads: AtomicUsize::new(0),
            }
        }

        fn unit_reads(&self) -> usize {
            self.unit_reads.load(Ordering::SeqCst)
        }
    }

    fn record(id: &str) -> NoteRecord {
        NoteRecord {
            id: id.to_string(),
            vault_id: "main".to_string(),
            section: "notes".to_string(),
            locus: None,
            status: "live".to_string(),
            body_text: format!("body of {id}"),
            author: None,
            tags_raw: None,
            content_hash: vec![0u8; 32],
            created: 1,
            updated: None,
            title: None,
        }
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }

    #[async_trait]
    impl DocumentStore for MinimalNoteStore {
        async fn write_note(&self, _note: &Note) -> Result<(), GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn get_content_hash(
            &self,
            _vault_id: &str,
            _id: NoteId,
        ) -> Result<Option<ContentHash>, GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn get_note(
            &self,
            _tenant_id: &str,
            note_id_ulid: &str,
        ) -> Result<Option<NoteRecord>, GradatumError> {
            self.unit_reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.rows.get(note_id_ulid).cloned())
        }

        async fn list_by_status(
            &self,
            _vault_id: &VaultId,
            _status: NoteStatus,
        ) -> Result<Vec<NoteId>, GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn downgrade_note(
            &self,
            _vault: &AclCheckedVaultId,
            _note_id: &NoteId,
            _reason: &str,
            _replaced_by: Option<&NoteId>,
        ) -> Result<(), GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn patch_note_status(
            &self,
            _vault: &AclCheckedVaultId,
            _note_id: &NoteId,
            _status: Option<&str>,
            _status_reason: Option<&str>,
            _replaced_by: Option<&NoteId>,
        ) -> Result<(), GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn update_note_locus(
            &self,
            _vault: &AclCheckedVaultId,
            _note_id: &NoteId,
            _locus: &crate::scope::LocusId,
        ) -> Result<(), GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn upsert_note_title(
            &self,
            _vault_id: &str,
            _note_id: &NoteId,
            _title: &str,
        ) -> Result<usize, GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn mark_forgotten(
            &self,
            _vault_id: &str,
            _note_id: &str,
            _reason: Option<&str>,
        ) -> Result<(), GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn reassert_forgotten(
            &self,
            _vault_id: &str,
            _note_id: &str,
        ) -> Result<(), GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn unmark_forgotten(
            &self,
            _vault_id: &str,
            _note_id: &str,
        ) -> Result<(), GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn list_forgotten(
            &self,
            _vault_id: &str,
            _limit: usize,
            _cursor: Option<&str>,
        ) -> Result<Vec<(String, Option<String>, String, i64, Option<String>)>, GradatumError>
        {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn count_forgotten(&self, _vault_id: &str) -> Result<usize, GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }

        async fn count_notes_by_status(
            &self,
            _vault_id: &str,
        ) -> Result<std::collections::HashMap<String, u64>, GradatumError> {
            unimplemented!("hors périmètre du test du défaut de get_notes_many")
        }
    }

    /// Le défaut rend les enregistrements dans l'ordre DEMANDÉ (jamais celui de la
    /// base), et omet silencieusement un identifiant inconnu.
    #[tokio::test]
    async fn get_notes_many_default_returns_records_in_requested_order_and_skips_unknown() {
        let store = MinimalNoteStore::with(&["a", "b", "c"]);

        let got = store
            .get_notes_many("main", &ids(&["c", "unknown", "a", "b"]))
            .await
            .expect("get_notes_many doit réussir");

        let order: Vec<&str> = got.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            order,
            vec!["c", "a", "b"],
            "l'ordre demandé prime ; un id inconnu est omis sans erreur"
        );
    }

    /// Un identifiant répété rend un seul enregistrement, à la position de sa
    /// PREMIÈRE occurrence.
    #[tokio::test]
    async fn get_notes_many_default_deduplicates_repeated_ids() {
        let store = MinimalNoteStore::with(&["a", "b"]);

        let got = store
            .get_notes_many("main", &ids(&["b", "a", "b", "a"]))
            .await
            .expect("get_notes_many ids répétés");

        let order: Vec<&str> = got.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(order, vec!["b", "a"], "un doublon ne rend pas deux lignes");
    }

    /// Le défaut coûte **une** lecture unitaire par identifiant DISTINCT — jamais
    /// une par entrée de la tranche. C'est le coût que l'override groupé supprime.
    #[tokio::test]
    async fn get_notes_many_default_reads_once_per_distinct_id() {
        let store = MinimalNoteStore::with(&["a", "b"]);

        let got = store
            .get_notes_many("main", &ids(&["a", "b", "a", "unknown"]))
            .await
            .expect("get_notes_many compteur");

        assert_eq!(got.len(), 2);
        assert_eq!(
            store.unit_reads(),
            3,
            "3 ids distincts (dont un inconnu) → 3 lectures unitaires, pas 4"
        );
    }

    /// Une tranche vide ne déclenche aucune lecture.
    #[tokio::test]
    async fn get_notes_many_default_empty_slice_reads_nothing() {
        let store = MinimalNoteStore::with(&["a"]);

        let got = store
            .get_notes_many("main", &[])
            .await
            .expect("get_notes_many vide");

        assert!(got.is_empty());
        assert_eq!(store.unit_reads(), 0);
    }
}
