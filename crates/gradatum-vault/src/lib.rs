//! # gradatum-vault
//!
//! Vault domain logic: registry + lifecycle + overrides + drift + effective_note cache.
//!
//! Composes the following lower-level crates:
//! - `gradatum-core`: primitives, traits, errors.
//! - `gradatum-markdown`: parse + write `.md`.
//! - `gradatum-cache`: `EffectiveNoteCache` (moka).
//! - `gradatum-index`: `SqliteIndex` implementing the `Index` trait.
//! - `gradatum-storage`: `FileStorage` (OpenDAL).
//!
//! ## Modules
//!
//! - [`registry`]: `Vault::create` / `Vault::open` — layout init, tenant_id, handles.
//! - [`lifecycle`]: `write_note` — ContentHash + persist `.md` + upsert index.
//! - [`overrides`]: `NoteMetadataOverride` — `Overridable` + `OverridePayload` impl.
//! - [`drift`]: `drift_check` — phase-A scan via `gradatum-index::scan_phase_a`.
//! - [`effective_note`]: `get_effective_note` — moka cache with checksum validation.
//! - [`history`]: `NoteHistoryEntry` — copy-on-write history entry.
//! - [`error`]: `VaultError` — strongly typed errors, no `Box<dyn Error>`.
//!
//! ## Stability
//!
//! `2.0.0` — public API under [SemVer 2.0.0](https://semver.org); backward-compatible additions only within `2.x`.
//! See [RELEASE-POLICY.md](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod drift;
pub mod effective_note;
pub mod error;
pub mod history;
pub mod lifecycle;
pub mod overrides;
pub mod registry;
pub mod write;

pub use error::VaultError;
pub use history::NoteHistoryEntry;
pub use lifecycle::{
    ARCHIVE_DIR_PREFIX, ArchiveOutcome, HISTORY_DIR_PREFIX, MAX_NOTE_TAGS, RestoreOutcome,
};
pub use overrides::NoteMetadataOverride;
pub use registry::Vault;
pub use write::WriteResult;

// ── Registry trait (T2 P2.0c) ────────────────────────────────────────────────

/// Vault registry access trait — exposed to `AppState` to decouple the server
/// from the concrete `Vault` implementation.
///
/// Async methods via `async_trait` — compatible with `Arc<dyn Registry>`.
///
/// ## Implementors
///
/// - [`Vault`]: real implementation backed by the SQLite index.
/// - `PlaceholderRegistry` (in `gradatum-server`): stub returning 0/0
///   for sync constructors before vault path injection.
#[async_trait::async_trait]
pub trait Registry: Send + Sync {
    /// Returns the number of tenants (distinct vault_id values) in the index.
    ///
    /// Returns 0 if the vault is empty or not yet initialised.
    async fn tenant_count(&self) -> Result<u32, gradatum_core::error::GradatumError>;

    /// Returns the number of distinct loci (vault_id + locus pairs) in the index.
    ///
    /// A locus is the sub-tenant organisational unit.
    /// Returns 0 if no notes are indexed.
    async fn locus_count(&self) -> Result<u32, gradatum_core::error::GradatumError>;

    /// Ensures a tenant exists in the registry.
    ///
    /// Idempotent — safe to call multiple times without side effects.
    async fn ensure_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<(), gradatum_core::error::GradatumError>;

    /// Reads a note by ULID string identifier from the vault.
    ///
    /// ## Behaviour
    ///
    /// - Valid cache hit → immediate return, `cache_hits` counter incremented.
    /// - Cache miss → `index.get_note` + `storage.read(.md)` + parse + cache insert.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` if the identifier is absent from the index.
    /// - `GradatumError::Storage` if the disk read fails.
    async fn read_note_by_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError>;

    /// Indicates whether a note is present in the index, regardless of the on-disk `.md`.
    ///
    /// Distinguishes a **phantom** note (index entry present but `.md` file absent —
    /// `read_note_by_id` returns `NoteNotFound`) from a **genuinely new** note (never
    /// indexed). Used by the server overwrite guard to reject a
    /// `vault_write { note_id = phantom, expected_sha256 = Some }` (the `expected_sha256`
    /// cannot be matched against any content) rather than accepting a request whose
    /// hash is unverifiable. This guard is a standalone rejection for the phantom case: it
    /// is complementary to — not a substitute for — the downstream compare-and-swap on a
    /// *live* note ([`Registry::write_if_match_internal`], see [`crate::write`]), which the
    /// phantom cannot use precisely because there is no on-disk content to hash against.
    ///
    /// ## Default
    ///
    /// `Ok(false)` — a registry without a real index (placeholder) knows no note.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::Storage` if the index query fails.
    async fn note_indexed(
        &self,
        _note_id: &str,
    ) -> Result<bool, gradatum_core::error::GradatumError> {
        Ok(false)
    }

    /// Lists the timestamps (Unix ms) of historical snapshots for a note.
    ///
    /// Returns a `Vec<i64>` sorted in ascending order (oldest first).
    /// Returns an empty list if no history exists or if the note is unknown.
    async fn history_versions(
        &self,
        note_id: &str,
    ) -> Result<Vec<i64>, gradatum_core::error::GradatumError>;

    /// Reads the content of a historical snapshot.
    ///
    /// `ts_ms` is a timestamp obtained from `history_versions`.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::Storage` if the snapshot is not found.
    /// - `GradatumError::Markdown` if parsing fails.
    async fn history_get(
        &self,
        note_id: &str,
        ts_ms: i64,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError>;

    /// Restores a note from a historical snapshot.
    ///
    /// Equivalent to writing the snapshot as the new current version (triggers a CoW).
    /// The note id is preserved. Returns the hex SHA-256 hash of the restored version.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::Storage` if the snapshot is not found.
    /// - `GradatumError::Markdown` if parsing the snapshot fails.
    ///
    /// ## Multi-vault isolation
    ///
    /// `checked` is the ACL-write witness ([`gradatum_core::scope::AclCheckedVaultId`]) of
    /// the TARGET vault, derived from the JWT on the handler side. A `Vault` instance
    /// serves exactly one physical vault, so a witness designating another vault yields
    /// `NoteNotFound` before any mutation — a closed oracle, matching the index predicate
    /// `WHERE vault_id = ?`.
    async fn history_restore(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        ts_ms: i64,
    ) -> Result<String, gradatum_core::error::GradatumError>;

    /// Computes a raw line-by-line diff between two versions.
    ///
    /// `a` and `b` are timestamps from `history_versions`, or `"current"` for
    /// the current version. Returns a list of diff lines (prefixed `-`/`+`/` `).
    ///
    /// Implementation: raw line-by-line diff (not Myers) — sufficient for
    /// MCP use (readability over compactness).
    async fn history_diff(
        &self,
        note_id: &str,
        a: &str,
        b: &str,
    ) -> Result<Vec<String>, gradatum_core::error::GradatumError>;

    /// Updates a note's status with state-machine validation.
    ///
    /// Only transitions defined in `NoteStatus::can_transition_to` are allowed.
    /// `target == current` is a silent no-op (idempotent).
    /// Each successful transition is recorded in `.history/` (copy-on-write).
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent.
    /// - `GradatumError::InvalidStatusTransition { from, to }` if the transition
    ///   is not allowed by the state machine.
    /// - `GradatumError::Storage` / `GradatumError::Markdown` on I/O error.
    ///
    /// ## Multi-vault isolation
    ///
    /// `checked` scopes the mutation to the TARGET vault: a witness belonging to another
    /// vault yields `NoteNotFound` before any transition — a closed oracle, matching the
    /// index predicate `WHERE vault_id = ?`.
    async fn update_note_status(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        target: gradatum_core::status::NoteStatus,
        reason: Option<String>,
    ) -> Result<(), gradatum_core::error::GradatumError>;

    /// Adds tags to a note — additive only, case-insensitive union semantics.
    ///
    /// The supplied `tags` are merged with existing tags; a tag already present
    /// (case-insensitive comparison) is ignored. No replacement or removal occurs.
    ///
    /// Idempotent: a second call with the same tags does not change state
    /// (a write/CoW is triggered only if the effective set changes —
    /// see the implementation).
    ///
    /// The write goes through the official CoW path (`write_note_with_id`):
    /// frontmatter updated + `.history/` snapshot recorded + FTS reindex via `upsert_note`.
    /// Searching by the new tag returns the note immediately after the call.
    ///
    /// ## Preconditions
    ///
    /// `tags` are assumed **already validated** by the caller (format, non-empty,
    /// cardinality). The implementation re-validates each tag via `Tag::new`
    /// (parse-don't-validate at the storage boundary) and returns `Validation` on failure.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent.
    /// - `GradatumError::Validation` if a tag is malformed.
    /// - `GradatumError::Storage` / `GradatumError::Markdown` on I/O error.
    ///
    /// ## Multi-vault isolation
    ///
    /// `checked` scopes the mutation to the TARGET vault: a witness belonging to another
    /// vault yields `NoteNotFound` before any tag is added — a closed oracle, matching the
    /// index predicate `WHERE vault_id = ?`.
    async fn add_tags(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        tags: &[String],
    ) -> Result<(), gradatum_core::error::GradatumError>;

    /// Physically moves a note to a new locus.
    ///
    /// Relocates the `.md` to `<tenant>/<new_locus>/<id>.md`, updates the index, and
    /// deletes the old orphan `.md`. After the call, `read_note_by_id` returns the new
    /// locus, so a read can no longer surface the previous one.
    ///
    /// Replaces the former index-only mutation (`SqliteIndex::update_note_locus`)
    /// used by the move handler. Idempotent: unchanged locus → no-op.
    ///
    /// ## Preconditions
    ///
    /// `new_locus` is assumed **already validated** by the caller (`LocusId::parse`).
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent.
    /// - `GradatumError::Storage` / `GradatumError::Markdown` on I/O error.
    ///
    /// ## Multi-vault isolation
    ///
    /// `checked` scopes the relocation to the TARGET vault: a witness belonging to another
    /// vault yields `NoteNotFound` before anything is moved — a closed oracle, matching the
    /// index predicate `WHERE vault_id = ?`.
    async fn move_locus(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), gradatum_core::error::GradatumError>;

    /// Writes a note using a caller-supplied ULID (internal API — server-to-worker path).
    ///
    /// Delegates to `Vault::write_note_with_id` on the concrete implementor.
    ///
    /// ## This is an unconditional write — NOT an optimistic lock
    ///
    /// The method takes no expected hash, and delegates without any compare-and-swap: an
    /// existing note at `id` is **overwritten silently**, whoever wrote it last. Do not
    /// rely on this call to detect a concurrent update.
    ///
    /// It is the **CREATE** half of the curated persist path (`expected_sha256 = None`):
    /// a fresh pre-allocated ULID written unconditionally. The **RMW** half
    /// (`expected_sha256 = Some`) goes through [`Registry::write_if_match_internal`]
    /// instead, which applies the compare-and-swap.
    ///
    /// The implementation still maps `VaultError::Conflict` to
    /// `GradatumError::Storage("conflict: hash mismatch")`, but that arm remains **dead on
    /// this method**: an unconditional write never produces `VaultError::Conflict` (see its
    /// own documentation in `crate::error`), so the variant is never observable here.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::Storage` if `frontmatter.vault_id` is non-empty and differs from
    ///   the `checked` ACL witness (internal inconsistency, fail-closed).
    /// - `GradatumError::Storage` / `GradatumError::Markdown` on I/O error.
    ///
    /// ## Multi-vault isolation
    ///
    /// `checked` is the witness ([`gradatum_core::scope::AclCheckedVaultId`]) of the TARGET
    /// vault of the write, derived from the ACL-checked tenant — on the loopback path it is
    /// propagated by the job pipeline from the authenticated `vault_write`. It forces every
    /// call site to build the witness through a named constructor (so omissions are
    /// greppable) and to write into the attested vault: `frontmatter.vault_id` MUST equal
    /// it, otherwise the call fails with `GradatumError::Storage` for internal
    /// inconsistency. No `vault_id` is ever hardcoded on this path.
    async fn write_note_with_id_internal(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        frontmatter: gradatum_core::frontmatter::Frontmatter,
        body: String,
        id: gradatum_core::identity::NoteId,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError>;

    /// Writes a note under an **optimistic lock** — the RMW half of the curated persist
    /// path (compare-and-swap).
    ///
    /// This is the production seam that wires [`Vault::write_if_match`] onto the
    /// `Arc<dyn Registry>` held by the server. The caller (the server's curated persist
    /// handler, reached through the internal persist API) routes here **only** when the
    /// request carries an `expected_sha256`; a request without one keeps taking the
    /// unconditional CREATE path ([`Registry::write_note_with_id_internal`]). That split
    /// preserves the `None → CREATE` / `Some → RMW` discrimination the field also encodes.
    ///
    /// ## Behaviour
    ///
    /// - `id` present on disk AND `expected_sha256 == current` → the note is rewritten and
    ///   [`WriteResult::Written`] is returned.
    /// - `id` present on disk AND `expected_sha256 != current` → **no write** — the note is
    ///   left intact and [`WriteResult::Conflict`] carries the current hash. The caller maps
    ///   this to a terminal `JobStatus::Conflict` (HTTP 409 over the internal API); it is a
    ///   success value, not an `Err`.
    /// - `id` absent (new or phantom `.md`) → written unconditionally (self-heal). The
    ///   server overwrite guard rejects the phantom + `expected_sha256` case upstream (409)
    ///   before any job is enqueued, so it does not reach here.
    ///
    /// ## Default implementation
    ///
    /// **Fail-loud** (`GradatumError::Storage`): a `Registry` that is not the real `Vault`
    /// (e.g. `PlaceholderRegistry` before vault injection) must never silently accept an
    /// optimistic-lock write. Same rationale as [`Registry::delete_note_by_id_in`].
    ///
    /// ## Errors
    ///
    /// - `GradatumError::Storage` if `frontmatter.vault_id` is non-empty and differs from
    ///   the `checked` ACL witness (internal inconsistency, fail-closed — parity with
    ///   [`Registry::write_note_with_id_internal`]).
    /// - `GradatumError::Storage` / `GradatumError::Markdown` on I/O error.
    /// - `GradatumError::Storage` from the fail-loud default.
    ///
    /// Note that a conflict is **not** an error — it is `Ok(WriteResult::Conflict)`.
    ///
    /// ## Multi-vault isolation
    ///
    /// `checked` is the ACL-write witness of the TARGET vault, exactly as for
    /// [`Registry::write_note_with_id_internal`]. On the live mono-vault deployment
    /// (`multi_tenant.enabled = false`, worker forcing `main`) the hash is read from and
    /// written to the same `main` vault. A writable cross-vault target is forbidden upstream
    /// (`resolve_write_namespace` → 403), so the read/write vault always coincide for every
    /// reachable input.
    async fn write_if_match_internal(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        frontmatter: gradatum_core::frontmatter::Frontmatter,
        body: String,
        id: gradatum_core::identity::NoteId,
        expected_sha256: [u8; 32],
    ) -> Result<crate::write::WriteResult, gradatum_core::error::GradatumError> {
        let _ = (checked, frontmatter, body, id, expected_sha256);
        Err(gradatum_core::error::GradatumError::Storage(
            "write_if_match_internal: Registry without a real vault (fail-loud)".to_string(),
        ))
    }

    /// Deletes a note and its history from the vault filesystem.
    ///
    /// Delegates to `Vault::delete_note` on the concrete implementor.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent.
    /// - `GradatumError::Storage` on I/O error.
    async fn delete_note_by_id(
        &self,
        id: gradatum_core::identity::NoteId,
    ) -> Result<(), gradatum_core::error::GradatumError>;

    /// Multi-vault variant of [`Registry::delete_note_by_id`]: deletes the
    /// `.md` + `.history/` of a note owned by `vault_id`. Delegates to
    /// [`Vault::delete_note_in`] on the concrete implementor.
    ///
    /// Default implementation is **fail-loud** (`GradatumError::Storage`) — a store that
    /// is not multi-vault-aware must never silently no-op a physical deletion (the exact
    /// bug this method closes: secondary-vault `.md` residue after a purge).
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent from the target vault.
    /// - `GradatumError::Storage` on I/O error, or from the fail-loud default.
    async fn delete_note_by_id_in(
        &self,
        vault_id: &str,
        id: gradatum_core::identity::NoteId,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        let _ = id;
        Err(gradatum_core::error::GradatumError::Storage(format!(
            "delete_note_by_id_in('{vault_id}') : Registry non multi-vault-aware (fail-loud)"
        )))
    }

    /// Archives a note: moves the `.md` and its `.history/` under `.archive/` and records
    /// a registry entry. Delegates to [`Vault::archive_note`].
    ///
    /// The note content is relocated, not erased: it stays readable on disk under
    /// `.archive/` until retention GC or an explicit purge destroys it.
    ///
    /// Does not de-index — the caller (server choke point) runs the index cascade.
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent.
    /// - `GradatumError::Storage` on I/O error moving the `.md`.
    async fn archive_note_by_id(
        &self,
        id: gradatum_core::identity::NoteId,
        archived_by: Option<String>,
        gc_due_ms: i64,
    ) -> Result<crate::lifecycle::ArchiveOutcome, gradatum_core::error::GradatumError>;

    /// Multi-vault variant of [`Registry::archive_note_by_id`]: archives a
    /// note owned by `vault_id`. Delegates to [`Vault::archive_note_in`] on the concrete
    /// implementor. Default implementation is **fail-loud** (same rationale as
    /// [`Registry::delete_note_by_id_in`]).
    ///
    /// ## Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent from the target vault.
    /// - `GradatumError::Storage` on I/O error, or from the fail-loud default.
    async fn archive_note_by_id_in(
        &self,
        vault_id: &str,
        id: gradatum_core::identity::NoteId,
        archived_by: Option<String>,
        gc_due_ms: i64,
    ) -> Result<crate::lifecycle::ArchiveOutcome, gradatum_core::error::GradatumError> {
        let _ = (id, archived_by, gc_due_ms);
        Err(gradatum_core::error::GradatumError::Storage(format!(
            "archive_note_by_id_in('{vault_id}') : Registry non multi-vault-aware (fail-loud)"
        )))
    }

    /// Runs the registry-driven archive retention GC:
    /// physically destroys archives past `gc_due` and marks `gc_at`. Delegates to
    /// [`Vault::run_archive_gc`]. Returns the number of archives destroyed.
    ///
    /// ## Errors
    ///
    /// - `GradatumError` only if the registry **selection** fails; per-entry failures
    ///   are absorbed (best-effort).
    async fn run_archive_gc(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<u64, gradatum_core::error::GradatumError>;

    /// Lists the archive registry entries matching `filter`.
    ///
    /// **Read-only** — this is the data source of the `vault_archives_list` MCP tool and
    /// public endpoint (agents and the operator *see* archives to prepare CLI commands).
    /// Delegates to [`gradatum_index::SqliteIndex::list_archive_entries`].
    ///
    /// ## Errors
    ///
    /// - `GradatumError::Storage` on a registry query failure.
    async fn list_archives(
        &self,
        filter: &gradatum_index::ArchiveListFilter,
    ) -> Result<Vec<gradatum_index::ArchiveEntry>, gradatum_core::error::GradatumError>;

    /// Resolves the **active** archive (neither GC'd nor restored) of a note, if any.
    /// Used by the admin restore/purge dry-run previews. Delegates to
    /// [`gradatum_index::SqliteIndex::get_active_archive`].
    ///
    /// ## Errors
    ///
    /// - `GradatumError::Storage` on a registry query failure.
    async fn get_active_archive(
        &self,
        note_id: &str,
    ) -> Result<Option<gradatum_index::ArchiveEntry>, gradatum_core::error::GradatumError>;

    /// Purges on demand the active archive of a note before the retention deadline
    /// (operator CLI): destroys the `.md` and `.history` files and marks `gc_at`, the
    /// registry row surviving as a trace. Delegates to [`Vault::purge_archive`].
    ///
    /// ## Errors
    ///
    /// - `GradatumError` if the registry resolution/marking fails.
    ///
    /// ## Returns
    ///
    /// `true` if an active archive was purged, `false` if none existed (idempotent).
    async fn purge_archive_by_id(
        &self,
        note_id: &str,
    ) -> Result<bool, gradatum_core::error::GradatumError>;

    /// Restores on demand the active archive of a note into **quarantine** (operator CLI):
    /// re-writes the `.md` at its original location, re-indexes it as `PendingReview` and
    /// marks `restored_at`. Delegates to [`Vault::restore_archive`].
    ///
    /// ## Errors
    ///
    /// - `GradatumError::InvalidInput` if `note_id` is not a valid ULID.
    /// - `GradatumError::NoteNotFound` if no active archive exists.
    /// - `GradatumError::Conflict` if the ULID is already present in the index (collision).
    /// - `GradatumError::Storage` / `GradatumError::Markdown` on I/O or parse failure.
    async fn restore_archive_by_id(
        &self,
        note_id: &str,
    ) -> Result<crate::lifecycle::RestoreOutcome, gradatum_core::error::GradatumError>;
}

#[async_trait::async_trait]
impl Registry for Vault {
    async fn tenant_count(&self) -> Result<u32, gradatum_core::error::GradatumError> {
        self.index.vault_id_count().await
    }

    async fn locus_count(&self) -> Result<u32, gradatum_core::error::GradatumError> {
        self.index.locus_count().await
    }

    async fn ensure_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        self.index.ensure_vault_id(tenant_id).await
    }

    async fn read_note_by_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;
        use ulid::Ulid;

        let ulid = Ulid::from_string(note_id).map_err(|e| {
            GradatumError::Storage(format!("read_note_by_id: invalid ULID {note_id:?}: {e}"))
        })?;
        let id = gradatum_core::identity::NoteId(ulid);

        self.read_note(id).await.map_err(|e| match e {
            crate::error::VaultError::Core(inner) => inner,
            crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
            crate::error::VaultError::Markdown(msg) => {
                GradatumError::Markdown(format!("read_note_by_id : {msg}"))
            }
            // Conflict ne peut pas survenir via read_note — variante défensive.
            crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                "read_note_by_id : unexpected conflict hash={:?}",
                hash
            )),
        })
    }

    async fn note_indexed(
        &self,
        note_id: &str,
    ) -> Result<bool, gradatum_core::error::GradatumError> {
        // Existence index-level (table `notes`), indépendante de la présence du `.md`.
        // `get_note_status` renvoie `Some(_)` pour toute ligne présente (fantôme inclus),
        // `None` si la note n'est pas indexée. Le `tenant_id` du vault est l'autorité
        // de scoping — cohérent avec `read_note_by_id`/`write_note_with_id` (sans tenant).
        let status = self
            .index
            .get_note_status(self.vault_id.as_str(), note_id)
            .await?;
        Ok(status.is_some())
    }

    async fn history_versions(
        &self,
        note_id: &str,
    ) -> Result<Vec<i64>, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;
        let id = self.parse_note_id(note_id)?;
        self.history_versions(id)
            .await
            .map_err(|e| GradatumError::Storage(format!("history_versions : {e}")))
    }

    async fn history_get(
        &self,
        note_id: &str,
        ts_ms: i64,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;
        let id = self.parse_note_id(note_id)?;
        self.history_get(id, ts_ms)
            .await
            .map_err(|e| GradatumError::Storage(format!("history_get : {e}")))
    }

    async fn history_restore(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        ts_ms: i64,
    ) -> Result<String, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;
        let id = self.parse_note_id(note_id)?;
        self.ensure_witness_owns_vault(checked, id)?;

        // Lire le snapshot puis l'écrire comme nouvelle version (déclenche un CoW).
        let snapshot = self
            .history_get(id, ts_ms)
            .await
            .map_err(|e| GradatumError::Storage(format!("history_restore get snapshot: {e}")))?;

        let written = self
            .write_note_with_id(snapshot.frontmatter, snapshot.body.markdown, id)
            .await
            .map_err(|e| GradatumError::Storage(format!("history_restore write: {e}")))?;

        // Retourner le hash hex de la version restaurée.
        Ok(written.content_hash.hex())
    }

    async fn history_diff(
        &self,
        note_id: &str,
        a: &str,
        b: &str,
    ) -> Result<Vec<String>, gradatum_core::error::GradatumError> {
        let id = self.parse_note_id(note_id)?;

        // Résoudre les deux versions : timestamp ou "current".
        let body_a = self.resolve_history_body(id, a).await?;
        let body_b = self.resolve_history_body(id, b).await?;

        // Diff brut ligne-à-ligne (PAS Myers — suffisant pour usage MCP).
        let lines_a: Vec<&str> = body_a.lines().collect();
        let lines_b: Vec<&str> = body_b.lines().collect();
        let diff = diff_lines_brut(&lines_a, &lines_b);
        Ok(diff)
    }

    async fn update_note_status(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        target: gradatum_core::status::NoteStatus,
        reason: Option<String>,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        let id = self.parse_note_id(note_id)?;
        self.ensure_witness_owns_vault(checked, id)?;

        self.update_status(id, target, reason)
            .await
            .map_err(|e| match e {
                crate::error::VaultError::Core(inner) => inner,
                crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
                crate::error::VaultError::Markdown(msg) => {
                    GradatumError::Markdown(format!("update_note_status : {msg}"))
                }
                // Conflict ne peut pas survenir via update_status — variante défensive.
                crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                    "update_note_status : unexpected conflict hash={:?}",
                    hash
                )),
            })
    }

    async fn add_tags(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        tags: &[String],
    ) -> Result<(), gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        let id = self.parse_note_id(note_id)?;
        self.ensure_witness_owns_vault(checked, id)?;

        self.add_tags(id, tags).await.map_err(|e| match e {
            crate::error::VaultError::Core(inner) => inner,
            crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
            crate::error::VaultError::Markdown(msg) => {
                GradatumError::Markdown(format!("add_tags : {msg}"))
            }
            // Conflict ne peut pas survenir via add_tags — variante défensive.
            crate::error::VaultError::Conflict(hash) => {
                GradatumError::Storage(format!("add_tags : unexpected conflict hash={:?}", hash))
            }
        })
    }

    async fn move_locus(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        note_id: &str,
        new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        let id = self.parse_note_id(note_id)?;
        self.ensure_witness_owns_vault(checked, id)?;

        self.move_locus(id, new_locus).await.map_err(|e| match e {
            crate::error::VaultError::Core(inner) => inner,
            crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
            crate::error::VaultError::Markdown(msg) => {
                GradatumError::Markdown(format!("move_locus : {msg}"))
            }
            // Conflict ne peut pas survenir via move_locus — variante défensive.
            crate::error::VaultError::Conflict(hash) => {
                GradatumError::Storage(format!("move_locus : unexpected conflict hash={:?}", hash))
            }
        })
    }

    async fn write_note_with_id_internal(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        frontmatter: gradatum_core::frontmatter::Frontmatter,
        body: String,
        id: gradatum_core::identity::NoteId,
    ) -> Result<gradatum_core::note::Note, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        // C4-1b (P0 security review) : le vault écrit DOIT être celui attesté ACL-write.
        // `frontmatter.vault_id` non vide et divergent du témoin = incohérence interne
        // (ex-hardcode `INTERNAL_TENANT_ID`) → refus fail-closed avant tout write.
        if !frontmatter.vault_id.as_str().is_empty()
            && frontmatter.vault_id.as_str() != checked.as_str()
        {
            return Err(GradatumError::Storage(format!(
                "write_note_with_id_internal: frontmatter vault_id '{}' ≠ ACL witness '{}' (fail-closed)",
                frontmatter.vault_id.as_str(),
                checked.as_str()
            )));
        }

        self.write_note_with_id(frontmatter, body, id)
            .await
            .map_err(|e| match e {
                crate::error::VaultError::Core(inner) => inner,
                crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
                crate::error::VaultError::Markdown(msg) => {
                    GradatumError::Markdown(format!("write_note_with_id_internal : {msg}"))
                }
                crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                    "conflict: hash mismatch — courant={}",
                    gradatum_core::identity::ContentHash(hash).hex()
                )),
            })
    }

    async fn write_if_match_internal(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        frontmatter: gradatum_core::frontmatter::Frontmatter,
        body: String,
        id: gradatum_core::identity::NoteId,
        expected_sha256: [u8; 32],
    ) -> Result<crate::write::WriteResult, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        // Parité fail-closed avec `write_note_with_id_internal` : le vault écrit DOIT être
        // celui attesté ACL-write. Un `frontmatter.vault_id` non vide et divergent du témoin
        // est une incohérence interne → refus avant tout write (C4-1b).
        if !frontmatter.vault_id.as_str().is_empty()
            && frontmatter.vault_id.as_str() != checked.as_str()
        {
            return Err(GradatumError::Storage(format!(
                "write_if_match_internal: frontmatter vault_id '{}' ≠ ACL witness '{}' (fail-closed)",
                frontmatter.vault_id.as_str(),
                checked.as_str()
            )));
        }

        // Délègue à la primitive testée F-41. Le conflit remonte en `Ok(WriteResult::Conflict)`
        // (valeur de succès), jamais en `Err` — seules les vraies erreurs I/O sont mappées.
        self.write_if_match(frontmatter, body, id, Some(expected_sha256))
            .await
            .map_err(|e| match e {
                crate::error::VaultError::Core(inner) => inner,
                crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
                crate::error::VaultError::Markdown(msg) => {
                    GradatumError::Markdown(format!("write_if_match_internal : {msg}"))
                }
                // `write_if_match` signale le conflit via WriteResult::Conflict, jamais via
                // VaultError::Conflict — cette branche reste défensive (never constructed).
                crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                    "write_if_match_internal : unexpected conflict hash={:?}",
                    hash
                )),
            })
    }

    async fn delete_note_by_id(
        &self,
        id: gradatum_core::identity::NoteId,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        self.delete_note(id).await.map_err(|e| match e {
            crate::error::VaultError::Core(inner) => inner,
            crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
            crate::error::VaultError::Markdown(msg) => {
                GradatumError::Markdown(format!("delete_note_by_id : {msg}"))
            }
            // Conflict ne peut survenir sur delete — variante défensive.
            crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                "delete_note_by_id : unexpected conflict hash={:?}",
                hash
            )),
        })
    }

    async fn archive_note_by_id(
        &self,
        id: gradatum_core::identity::NoteId,
        archived_by: Option<String>,
        gc_due_ms: i64,
    ) -> Result<crate::lifecycle::ArchiveOutcome, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        self.archive_note(id, archived_by, gc_due_ms)
            .await
            .map_err(|e| match e {
                crate::error::VaultError::Core(inner) => inner,
                crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
                crate::error::VaultError::Markdown(msg) => {
                    GradatumError::Markdown(format!("archive_note_by_id : {msg}"))
                }
                crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                    "archive_note_by_id : unexpected conflict hash={hash:?}"
                )),
            })
    }

    async fn delete_note_by_id_in(
        &self,
        vault_id: &str,
        id: gradatum_core::identity::NoteId,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        self.delete_note_in(vault_id, id)
            .await
            .map_err(|e| match e {
                crate::error::VaultError::Core(inner) => inner,
                crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
                crate::error::VaultError::Markdown(msg) => {
                    GradatumError::Markdown(format!("delete_note_by_id_in : {msg}"))
                }
                crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                    "delete_note_by_id_in : unexpected conflict hash={hash:?}"
                )),
            })
    }

    async fn archive_note_by_id_in(
        &self,
        vault_id: &str,
        id: gradatum_core::identity::NoteId,
        archived_by: Option<String>,
        gc_due_ms: i64,
    ) -> Result<crate::lifecycle::ArchiveOutcome, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        self.archive_note_in(vault_id, id, archived_by, gc_due_ms)
            .await
            .map_err(|e| match e {
                crate::error::VaultError::Core(inner) => inner,
                crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
                crate::error::VaultError::Markdown(msg) => {
                    GradatumError::Markdown(format!("archive_note_by_id_in : {msg}"))
                }
                crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                    "archive_note_by_id_in : unexpected conflict hash={hash:?}"
                )),
            })
    }

    async fn run_archive_gc(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<u64, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        // Appel explicite de la méthode inhérente (même nom que ce trait method).
        Vault::run_archive_gc(self, now_ms, limit)
            .await
            .map_err(|e| match e {
                crate::error::VaultError::Core(inner) => inner,
                crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
                crate::error::VaultError::Markdown(msg) => {
                    GradatumError::Markdown(format!("run_archive_gc : {msg}"))
                }
                crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                    "run_archive_gc : unexpected conflict hash={hash:?}"
                )),
            })
    }

    async fn list_archives(
        &self,
        filter: &gradatum_index::ArchiveListFilter,
    ) -> Result<Vec<gradatum_index::ArchiveEntry>, gradatum_core::error::GradatumError> {
        self.index.list_archive_entries(filter).await
    }

    async fn get_active_archive(
        &self,
        note_id: &str,
    ) -> Result<Option<gradatum_index::ArchiveEntry>, gradatum_core::error::GradatumError> {
        self.index
            .get_active_archive(self.vault_id.as_str(), note_id)
            .await
    }

    async fn purge_archive_by_id(
        &self,
        note_id: &str,
    ) -> Result<bool, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        self.purge_archive(note_id).await.map_err(|e| match e {
            crate::error::VaultError::Core(inner) => inner,
            crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
            crate::error::VaultError::Markdown(msg) => {
                GradatumError::Markdown(format!("purge_archive_by_id : {msg}"))
            }
            crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                "purge_archive_by_id : unexpected conflict hash={hash:?}"
            )),
        })
    }

    async fn restore_archive_by_id(
        &self,
        note_id: &str,
    ) -> Result<crate::lifecycle::RestoreOutcome, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        // Le handler valide déjà l'ULID (InvalidInput) ; on re-mappe ici pour honorer le
        // contrat du trait (`parse_note_id` renvoie sinon un `Storage`).
        let id = self.parse_note_id(note_id).map_err(|_| {
            GradatumError::InvalidInput(format!("invalid note_id (ULID expected): {note_id:?}"))
        })?;

        self.restore_archive(id).await.map_err(|e| match e {
            crate::error::VaultError::Core(inner) => inner,
            crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
            crate::error::VaultError::Markdown(msg) => {
                GradatumError::Markdown(format!("restore_archive_by_id : {msg}"))
            }
            crate::error::VaultError::Conflict(hash) => GradatumError::Storage(format!(
                "restore_archive_by_id : unexpected conflict hash={hash:?}"
            )),
        })
    }
}

impl Vault {
    /// Multi-vault isolation gate for vault-layer mutations.
    ///
    /// A [`Vault`] instance serves exactly ONE physical vault (`self.vault_id`, fixed at
    /// startup). Mutations addressed by ULID — `move_locus`, `add_tags`,
    /// `update_note_status`, `history_restore` — resolve the note under that tenant, so
    /// without this gate a legitimate third-party tenant (holding a JWT and a grant on ITS
    /// own vault) could mutate a note of another vault simply by targeting its ULID.
    ///
    /// The [`AclCheckedVaultId`] witness carries the TARGET vault derived from the JWT. If
    /// it does not designate the vault served by this instance, the note cannot belong to
    /// it and the call returns `NoteNotFound` BEFORE any mutation. The oracle stays closed
    /// — the answer is indistinguishable from a non-existent ULID — and it mirrors the
    /// index-side `WHERE vault_id = ?` pinning. When a single vault is in play the witness
    /// always matches, so the gate is transparent.
    fn ensure_witness_owns_vault(
        &self,
        checked: &gradatum_core::scope::AclCheckedVaultId,
        id: gradatum_core::identity::NoteId,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        if checked.as_str() != self.vault_id.as_str() {
            return Err(gradatum_core::error::GradatumError::NoteNotFound(id));
        }
        Ok(())
    }

    /// Internal helper: parses a ULID string into a `NoteId`.
    fn parse_note_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_core::identity::NoteId, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("invalid ULID {note_id:?}: {e}")))?;
        Ok(gradatum_core::identity::NoteId(ulid))
    }

    /// Internal helper: resolves a version selector (`"current"` or a millisecond timestamp) into a body `String`.
    async fn resolve_history_body(
        &self,
        id: gradatum_core::identity::NoteId,
        version_selector: &str,
    ) -> Result<String, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;
        if version_selector == "current" {
            let note = self.read_note(id).await.map_err(|e| {
                GradatumError::Storage(format!("resolve_history_body current: {e}"))
            })?;
            Ok(note.body.markdown)
        } else {
            let ts_ms = version_selector.parse::<i64>().map_err(|_| {
                GradatumError::Storage(format!(
                    "invalid version selector: expected 'current' or timestamp ms, got {:?}",
                    version_selector
                ))
            })?;
            let snapshot = self.history_get(id, ts_ms).await.map_err(|e| {
                GradatumError::Storage(format!("resolve_history_body snapshot: {e}"))
            })?;
            Ok(snapshot.body.markdown)
        }
    }
}

/// Computes a raw line-by-line diff between two note bodies.
///
/// Algorithm: simplified LCS — line present in A but absent in B = `-`,
/// line present in B but absent in A = `+`, common line = ` `.
///
/// Note: not a Myers diff (no optimal block alignment).
/// Sufficient for MCP use (human inspection of changes).
fn diff_lines_brut(lines_a: &[&str], lines_b: &[&str]) -> Vec<String> {
    // Diff naïf : compare position par position, signale les divergences.
    // Pour les notes de vault (généralement < 200 lignes), O(n) est acceptable.
    let max_len = lines_a.len().max(lines_b.len());
    let mut result = Vec::with_capacity(max_len * 2);

    let mut i = 0;
    let mut j = 0;

    while i < lines_a.len() || j < lines_b.len() {
        match (lines_a.get(i), lines_b.get(j)) {
            (Some(la), Some(lb)) => {
                if la == lb {
                    result.push(format!(" {}", la));
                    i += 1;
                    j += 1;
                } else {
                    result.push(format!("-{}", la));
                    result.push(format!("+{}", lb));
                    i += 1;
                    j += 1;
                }
            }
            (Some(la), None) => {
                result.push(format!("-{}", la));
                i += 1;
            }
            (None, Some(lb)) => {
                result.push(format!("+{}", lb));
                j += 1;
            }
            (None, None) => break,
        }
    }

    result
}

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
