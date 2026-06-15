//! # gradatum-vault
//!
//! Vault domain logic: registry + lifecycle + overrides + drift + effective_note cache.
//!
//! Layer L2 of the Gradatum architecture — composes the following L1 crates:
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
//! `0.x` — no API stability guarantees.
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
pub use lifecycle::{HISTORY_DIR_PREFIX, MAX_NOTE_TAGS};
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
    async fn history_restore(
        &self,
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
    async fn update_note_status(
        &self,
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
    async fn add_tags(
        &self,
        note_id: &str,
        tags: &[String],
    ) -> Result<(), gradatum_core::error::GradatumError>;

    /// Physically moves a note to a new locus.
    ///
    /// Relocates the `.md` to `<tenant>/<new_locus>/<id>.md`, updates the index, and
    /// deletes the old orphan `.md`. After the call, `read_note_by_id` returns the
    /// new locus (`vault_read` consistency — stale-locus caveat eliminated).
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
    async fn move_locus(
        &self,
        note_id: &str,
        new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), gradatum_core::error::GradatumError>;
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
            GradatumError::Storage(format!("read_note_by_id : ULID invalide {note_id:?} : {e}"))
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
                "read_note_by_id : conflit inattendu hash={:?}",
                hash
            )),
        })
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
        note_id: &str,
        ts_ms: i64,
    ) -> Result<String, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;
        let id = self.parse_note_id(note_id)?;

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
        note_id: &str,
        target: gradatum_core::status::NoteStatus,
        reason: Option<String>,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        let id = self.parse_note_id(note_id)?;

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
                    "update_note_status : conflit inattendu hash={:?}",
                    hash
                )),
            })
    }

    async fn add_tags(
        &self,
        note_id: &str,
        tags: &[String],
    ) -> Result<(), gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        let id = self.parse_note_id(note_id)?;

        self.add_tags(id, tags).await.map_err(|e| match e {
            crate::error::VaultError::Core(inner) => inner,
            crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
            crate::error::VaultError::Markdown(msg) => {
                GradatumError::Markdown(format!("add_tags : {msg}"))
            }
            // Conflict ne peut pas survenir via add_tags — variante défensive.
            crate::error::VaultError::Conflict(hash) => {
                GradatumError::Storage(format!("add_tags : conflit inattendu hash={:?}", hash))
            }
        })
    }

    async fn move_locus(
        &self,
        note_id: &str,
        new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;

        let id = self.parse_note_id(note_id)?;

        self.move_locus(id, new_locus).await.map_err(|e| match e {
            crate::error::VaultError::Core(inner) => inner,
            crate::error::VaultError::Storage(msg) => GradatumError::Storage(msg),
            crate::error::VaultError::Markdown(msg) => {
                GradatumError::Markdown(format!("move_locus : {msg}"))
            }
            // Conflict ne peut pas survenir via move_locus — variante défensive.
            crate::error::VaultError::Conflict(hash) => {
                GradatumError::Storage(format!("move_locus : conflit inattendu hash={:?}", hash))
            }
        })
    }
}

impl Vault {
    /// Internal helper: parses a ULID string into a `NoteId`.
    fn parse_note_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_core::identity::NoteId, gradatum_core::error::GradatumError> {
        use gradatum_core::error::GradatumError;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("ULID invalide {note_id:?} : {e}")))?;
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
                    "sélecteur de version invalide : attendu 'current' ou timestamp ms, reçu {:?}",
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
