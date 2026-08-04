//! Archive registry — the `archive_index` table.
//!
//! Deleting a note archives it and writes one row here. Retention garbage collection is
//! **driven by this registry**, never by a filesystem scan. A row outlives the data it
//! describes: it stays after physical destruction (`gc_at` set) and after restoration
//! (`restored_at` set). This is an archive history, not a work queue.
//!
//! The registry is **separate** from the search indexes: an archived note is invisible to
//! `vault_search` and `vault_read`, and nothing here is ever joined into a search query.

use gradatum_core::error::GradatumError;
use rusqlite::types::Value as SqlVal;

use crate::sqlite::SqliteIndex;

/// One row of the archive registry (`archive_index`).
///
/// `gc_at`/`restored_at` are `None` for an **active** archive (still recoverable). At most
/// one active row exists per `(vault_id, note_id)` pair, enforced by a partial unique index
/// in the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// ULID of the archived note.
    pub note_id: String,
    /// Vault owning the archive (mirrors `notes.vault_id`).
    pub vault_id: String,
    /// Canonical section the note came from (kebab-case).
    pub section: String,
    /// H1 title as of archiving time, when known.
    pub title: Option<String>,
    /// Original locus (sub-directory); `None` means the tenant root.
    pub original_locus: Option<String>,
    /// Path of the archived `.md` file, relative to the vault root.
    pub archive_path: String,
    /// Archiving timestamp (epoch milliseconds, UTC).
    pub archived_at: i64,
    /// `sub` claim of the token that triggered the archiving, when known.
    pub archived_by: Option<String>,
    /// Retention deadline (`archived_at + retention`); GC destroys the files past it.
    pub gc_due: i64,
    /// Physical destruction timestamp (epoch ms); `None` while the files still exist.
    pub gc_at: Option<i64>,
    /// Restoration timestamp (epoch ms); `None` if the archive was never restored.
    pub restored_at: Option<i64>,
}

/// Read-only listing filter for the registry (`vault_archives_list`, operator CLI).
///
/// With the defaults (`include_gc = include_restored = false`) only **active** archives are
/// returned. `limit` is clamped by a hard cap when the query runs.
#[derive(Debug, Clone)]
pub struct ArchiveListFilter {
    /// Owning-vault filter; `None` means every vault.
    pub vault_id: Option<String>,
    /// Section filter (kebab-case); `None` means every section.
    pub section: Option<String>,
    /// Lower bound `archived_at >= from_ms`; `None` means unbounded.
    pub from_ms: Option<i64>,
    /// Upper bound `archived_at <= until_ms`; `None` means unbounded.
    pub until_ms: Option<i64>,
    /// Also return archives already destroyed (`gc_at IS NOT NULL`).
    pub include_gc: bool,
    /// Also return archives already restored (`restored_at IS NOT NULL`).
    pub include_restored: bool,
    /// Maximum number of rows, clamped to [`ARCHIVE_LIST_MAX`].
    pub limit: usize,
    /// Pagination offset.
    pub offset: usize,
}

impl Default for ArchiveListFilter {
    fn default() -> Self {
        Self {
            vault_id: None,
            section: None,
            from_ms: None,
            until_ms: None,
            include_gc: false,
            include_restored: false,
            limit: 50,
            offset: 0,
        }
    }
}

/// Hard cap on the number of rows a listing may return (denial-of-service guard).
pub const ARCHIVE_LIST_MAX: usize = 500;

/// Maps one SQL row, in the fixed column order, into an [`ArchiveEntry`].
fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArchiveEntry> {
    Ok(ArchiveEntry {
        note_id: row.get(0)?,
        vault_id: row.get(1)?,
        section: row.get(2)?,
        title: row.get(3)?,
        original_locus: row.get(4)?,
        archive_path: row.get(5)?,
        archived_at: row.get(6)?,
        archived_by: row.get(7)?,
        gc_due: row.get(8)?,
        gc_at: row.get(9)?,
        restored_at: row.get(10)?,
    })
}

/// Projected columns, in the order [`row_to_entry`] expects.
const ARCHIVE_COLS: &str = "note_id, vault_id, section, title, original_locus, archive_path, \
     archived_at, archived_by, gc_due, gc_at, restored_at";

impl SqliteIndex {
    /// Records a new active archive in the registry.
    ///
    /// # Errors
    ///
    /// - [`GradatumError::Storage`] on SQL failure. The partial unique index
    ///   `uidx_archive_active` covers `(vault_id, note_id)`, so a second active archive for
    ///   the same pair is rejected and surfaces as `Storage` (callers are expected to
    ///   uphold that invariant upstream). Two distinct vaults may hold an active archive
    ///   for the same ULID at the same time, so one vault cannot block another.
    pub async fn insert_archive_entry(&self, entry: &ArchiveEntry) -> Result<(), GradatumError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO archive_index
                 (note_id, vault_id, section, title, original_locus, archive_path,
                  archived_at, archived_by, gc_due, gc_at, restored_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                entry.note_id,
                entry.vault_id,
                entry.section,
                entry.title,
                entry.original_locus,
                entry.archive_path,
                entry.archived_at,
                entry.archived_by,
                entry.gc_due,
                entry.gc_at,
                entry.restored_at,
            ],
        )
        .map_err(|e| GradatumError::Storage(format!("insert_archive_entry : {e}")))?;
        Ok(())
    }

    /// Lists registry rows matching `filter` (read-only, paginated).
    ///
    /// Ordered by `archived_at DESC` (most recent first). `limit` is clamped to
    /// [`ARCHIVE_LIST_MAX`].
    ///
    /// # Errors
    ///
    /// [`GradatumError::Storage`] on SQL failure.
    pub async fn list_archive_entries(
        &self,
        filter: &ArchiveListFilter,
    ) -> Result<Vec<ArchiveEntry>, GradatumError> {
        let conn = self.conn.lock().await;

        let mut sql = format!("SELECT {ARCHIVE_COLS} FROM archive_index WHERE 1=1");
        let mut binds: Vec<SqlVal> = Vec::new();
        if !filter.include_gc {
            sql.push_str(" AND gc_at IS NULL");
        }
        if !filter.include_restored {
            sql.push_str(" AND restored_at IS NULL");
        }
        if let Some(vault_id) = filter.vault_id.as_ref() {
            sql.push_str(" AND vault_id = ?");
            binds.push(SqlVal::Text(vault_id.clone()));
        }
        if let Some(section) = filter.section.as_ref() {
            sql.push_str(" AND section = ?");
            binds.push(SqlVal::Text(section.clone()));
        }
        if let Some(from) = filter.from_ms {
            sql.push_str(" AND archived_at >= ?");
            binds.push(SqlVal::Integer(from));
        }
        if let Some(until) = filter.until_ms {
            sql.push_str(" AND archived_at <= ?");
            binds.push(SqlVal::Integer(until));
        }
        sql.push_str(" ORDER BY archived_at DESC, id DESC LIMIT ? OFFSET ?");
        let limit = filter.limit.min(ARCHIVE_LIST_MAX) as i64;
        binds.push(SqlVal::Integer(limit));
        binds.push(SqlVal::Integer(filter.offset as i64));

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| GradatumError::Storage(format!("prepare list_archive_entries : {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds), row_to_entry)
            .map_err(|e| GradatumError::Storage(format!("query list_archive_entries : {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(
                r.map_err(|e| GradatumError::Storage(format!("row list_archive_entries : {e}")))?,
            );
        }
        Ok(out)
    }

    /// Resolves the **active** archive (neither destroyed nor restored) of a note owned by
    /// `vault_id`.
    ///
    /// Used by restore and purge by ULID. Returns `None` when no active archive belongs to
    /// `vault_id` — **even if ANOTHER vault holds an active archive for the same ULID**.
    /// ULIDs may collide across vaults, so filtering on `vault_id` closes a cross-vault
    /// information leak; it is defence in depth behind the guard on the vault side.
    ///
    /// # Errors
    ///
    /// [`GradatumError::Storage`] on SQL failure.
    pub async fn get_active_archive(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<ArchiveEntry>, GradatumError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ARCHIVE_COLS} FROM archive_index
                 WHERE note_id = ?1 AND vault_id = ?2 AND gc_at IS NULL AND restored_at IS NULL
                 LIMIT 1"
            ))
            .map_err(|e| GradatumError::Storage(format!("prepare get_active_archive : {e}")))?;
        let mut rows = stmt
            .query_map(rusqlite::params![note_id, vault_id], row_to_entry)
            .map_err(|e| GradatumError::Storage(format!("query get_active_archive : {e}")))?;
        match rows.next() {
            Some(r) => Ok(Some(r.map_err(|e| {
                GradatumError::Storage(format!("row get_active_archive : {e}"))
            })?)),
            None => Ok(None),
        }
    }

    /// Selects archives whose retention has expired and that are not destroyed yet
    /// (`gc_due < now AND gc_at IS NULL AND restored_at IS NULL`).
    ///
    /// This is the query that drives retention GC. `limit` bounds the batch size and is
    /// itself clamped to [`ARCHIVE_LIST_MAX`].
    ///
    /// # Errors
    ///
    /// [`GradatumError::Storage`] on SQL failure.
    pub async fn select_gc_due_archives(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<ArchiveEntry>, GradatumError> {
        let conn = self.conn.lock().await;
        let capped = limit.min(ARCHIVE_LIST_MAX) as i64;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ARCHIVE_COLS} FROM archive_index
                 WHERE gc_due < ?1 AND gc_at IS NULL AND restored_at IS NULL
                 ORDER BY gc_due ASC LIMIT ?2"
            ))
            .map_err(|e| GradatumError::Storage(format!("prepare select_gc_due_archives : {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![now_ms, capped], row_to_entry)
            .map_err(|e| GradatumError::Storage(format!("query select_gc_due_archives : {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| {
                GradatumError::Storage(format!("row select_gc_due_archives : {e}"))
            })?);
        }
        Ok(out)
    }

    /// Marks the active archive of a note owned by `vault_id` as destroyed (`gc_at = now`).
    ///
    /// The row itself SURVIVES as a trace. Returns `true` when an active row **belonging to
    /// `vault_id`** was marked — never one belonging to another vault, since ULIDs may
    /// collide across vaults and the scoping prevents cross-vault tampering.
    ///
    /// # Errors
    ///
    /// [`GradatumError::Storage`] on SQL failure.
    pub async fn mark_archive_gc(
        &self,
        vault_id: &str,
        note_id: &str,
        gc_at_ms: i64,
    ) -> Result<bool, GradatumError> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE archive_index SET gc_at = ?3
                 WHERE note_id = ?1 AND vault_id = ?2 AND gc_at IS NULL AND restored_at IS NULL",
                rusqlite::params![note_id, vault_id, gc_at_ms],
            )
            .map_err(|e| GradatumError::Storage(format!("mark_archive_gc : {e}")))?;
        Ok(n > 0)
    }

    /// Marks the active archive of a note owned by `vault_id` as restored
    /// (`restored_at = now`).
    ///
    /// The row itself SURVIVES as a trace. Returns `true` when an active row **belonging to
    /// `vault_id`** was marked — never one belonging to another vault, since ULIDs may
    /// collide across vaults and the scoping prevents cross-vault tampering.
    ///
    /// # Errors
    ///
    /// [`GradatumError::Storage`] on SQL failure.
    pub async fn mark_archive_restored(
        &self,
        vault_id: &str,
        note_id: &str,
        restored_at_ms: i64,
    ) -> Result<bool, GradatumError> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE archive_index SET restored_at = ?3
                 WHERE note_id = ?1 AND vault_id = ?2 AND gc_at IS NULL AND restored_at IS NULL",
                rusqlite::params![note_id, vault_id, restored_at_ms],
            )
            .map_err(|e| GradatumError::Storage(format!("mark_archive_restored : {e}")))?;
        Ok(n > 0)
    }
}
