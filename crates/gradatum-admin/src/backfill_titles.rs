//! `gradatum-admin backfill-titles` sub-command.
//!
//! Iterates notes where `title IS NULL` in a Gradatum SQLite database,
//! extracts the Markdown H1 via `extract_h1_title`, and updates the `title` column.
//!
//! ## Usage
//! ```text
//! gradatum-admin backfill-titles --root /var/lib/gradatum --dry-run
//! gradatum-admin backfill-titles --root /var/lib/gradatum --tenant main
//! ```
//!
//! Idempotent: re-running on an already back-filled database makes no changes
//! (`WHERE title IS NULL` returns 0 results).
//!
//! A backup is strongly recommended before running against a production database.

use anyhow::{Context, Result};
use gradatum_core::paths::vault_index_path;
use std::path::PathBuf;

/// Arguments for the `backfill-titles` sub-command.
#[derive(Debug, Clone)]
pub struct BackfillTitlesArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`).
    pub root: PathBuf,
    /// Target tenant (default: `"main"`).
    pub tenant: String,
    /// Dry-run mode: computes titles without persisting them.
    pub dry_run: bool,
    /// Maximum number of notes to process; unlimited when absent.
    pub limit: Option<usize>,
}

/// Report for a title back-fill run.
#[derive(Debug, Default, Clone)]
pub struct BackfillTitlesReport {
    /// Number of title-less notes scanned.
    pub notes_scanned: usize,
    /// Number of notes for which an H1 was successfully extracted.
    pub titles_extracted: usize,
    /// Number of notes actually updated (always `0` in dry-run mode).
    pub titles_updated: usize,
    /// Notes with no valid H1 (absent or empty after stripping).
    pub titles_no_h1: usize,
}

/// Back-fills missing titles via `extract_h1_title`.
///
/// Iterates notes where `title IS NULL` for the given tenant, extracts the
/// Markdown H1, and updates the `title` column. Notes with no valid H1 are
/// silently skipped (counted in `titles_no_h1`).
///
/// # Errors
///
/// Returns an error if the database is inaccessible or if an UPDATE fails.
///
/// # Notes
///
/// - Idempotent: re-run on an already back-filled database yields `titles_updated = 0`.
/// - Empty-H1 skip is required: `Some("")` is semantically distinct from NULL;
///   writing `title = ""` would prevent a second pass because `WHERE title IS NULL`
///   would no longer select that note.
pub async fn backfill_titles(args: BackfillTitlesArgs) -> Result<BackfillTitlesReport> {
    // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
    let db_path = vault_index_path(&args.root);

    if !db_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le worker doit avoir démarré au moins une fois",
            db_path.display()
        );
    }

    tokio::task::spawn_blocking(move || {
        run_backfill_sync(&db_path, &args.tenant, args.dry_run, args.limit)
    })
    .await
    .context("spawn_blocking backfill_titles")?
}

/// Synchronous back-fill implementation; called from `spawn_blocking`.
fn run_backfill_sync(
    db_path: &std::path::Path,
    tenant: &str,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<BackfillTitlesReport> {
    let conn =
        rusqlite::Connection::open(db_path).context("ouverture index.db pour backfill-titles")?;

    // Enable WAL for concurrent read/write with gradatum-server.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .context("PRAGMA journal_mode=WAL")?;

    let limit_clause = limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    let query = format!(
        "SELECT id, body_text FROM notes \
         WHERE (title IS NULL OR title = '') AND vault_id = ?1 \
         ORDER BY created ASC \
         {limit_clause}"
    );

    // Collect into a Vec to release the Statement before the UPDATEs.
    let mut stmt = conn
        .prepare(&query)
        .context("préparation SELECT notes sans titre")?;

    let rows: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![tenant], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("exécution SELECT notes sans titre")?
        .collect::<std::result::Result<_, _>>()
        .context("collecte notes sans titre")?;

    drop(stmt);

    let notes_scanned = rows.len();
    let mut report = BackfillTitlesReport {
        notes_scanned,
        ..Default::default()
    };

    if dry_run {
        // Dry-run: compute stats without writing to the database.
        for (_id, body) in &rows {
            // `extract_h1_title` returns `None` for an absent or empty H1
            // ("# " → `None`; empty-title filtering is handled inside the function).
            match gradatum_curator::extract_h1_title(body) {
                Some(_) => {
                    report.titles_extracted += 1;
                    // titles_updated stays 0: dry-run, no writes.
                }
                None => {
                    report.titles_no_h1 += 1;
                }
            }
        }
        return Ok(report);
    }

    // Apply mode: UPDATE inside a single transaction.
    let tx = conn
        .unchecked_transaction()
        .context("début transaction backfill-titles")?;

    for (id, body) in &rows {
        // `extract_h1_title` returns `Option<String>` with built-in empty filtering —
        // no `!is_empty()` guard needed.
        match gradatum_curator::extract_h1_title(body) {
            Some(title) => {
                report.titles_extracted += 1;
                tx.execute(
                    "UPDATE notes SET title = ?1 WHERE id = ?2",
                    rusqlite::params![title, id],
                )
                .context("UPDATE title")?;
                report.titles_updated += 1;
            }
            None => {
                // H1 absent or empty after stripping → silently skip.
                report.titles_no_h1 += 1;
            }
        }
    }

    tx.commit().context("commit transaction backfill-titles")?;

    Ok(report)
}
