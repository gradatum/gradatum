//! `gradatum-admin repair-note-links` sub-command (F-147).
//!
//! Reconciles the outgoing `note_links` edges of every relevant LIVE note against the
//! edges recomputed from its **current body** via
//! [`gradatum_curator::wikilinks_sync::resolve_wikilinks_sync`]: it DELETEs stale edges
//! (present in the table, absent from the body) and INSERTs missing ones, so the graph is
//! an accurate reflection of the current bodies rather than a cumulative history.
//!
//! ## Why it exists — difference with `backfill-note-links`
//!
//! [`crate::backfill_note_links`] is a one-shot *seeder*: it scans only notes that have
//! **zero** outgoing edges and **only inserts** what is missing — it never removes
//! anything. It therefore cannot fix a note whose body changed a link's target: the old
//! edge was written with `INSERT OR IGNORE` and, with no matching `DELETE`, stayed
//! forever (the F-147 accumulation defect).
//!
//! This command is the curative *reconciler*: it scans every note that has a wikilink in
//! its body **or** an existing edge, and it both **removes** stale edges and **inserts**
//! missing ones.
//!
//! ## Source of truth
//!
//! The note body — never insertion order, never `created_at`. "Keep the most recent edge"
//! is a WRONG heuristic (two edges to different targets are equally "real" in the table);
//! the only authority is what the current body links to, resolved with the exact same
//! logic the worker uses at write time.
//!
//! ## Dry-run
//!
//! The operator-facing default (`main.rs`) is a dry-run: it resolves and diffs, prints the
//! exact edges it *would* delete/add, and writes nothing. The operator inspects that diff
//! before mutating anything. `--execute` performs the reconciliation for real.
//!
//! ## Idempotence
//!
//! A second pass over an already-reconciled database resolves the same edges, computes
//! empty add/remove sets, and writes nothing.
//!
//! ## Usage
//! ```text
//! gradatum-admin repair-note-links --root /var/lib/gradatum --tenant main            # dry-run
//! gradatum-admin repair-note-links --root /var/lib/gradatum --tenant main --execute  # mutate
//! ```

use anyhow::{Context, Result};
use gradatum_core::paths::vault_index_path;
use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::backfill_note_links::escape_like_pattern;

/// Arguments for the `repair-note-links` sub-command.
#[derive(Debug, Clone)]
pub struct RepairNoteLinksArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`).
    pub root: PathBuf,
    /// Target tenant (default: `"main"`).
    pub tenant: String,
    /// Dry-run mode: resolves and diffs the candidate notes without writing anything.
    ///
    /// The default in `main.rs` is `true` (safe): reconciliation deletes edges, so mutation
    /// is opt-in via `--execute`.
    pub dry_run: bool,
    /// Maximum number of notes to process; unlimited when absent.
    pub limit: Option<usize>,
}

/// Report of a `note_links` reconciliation run.
#[derive(Debug, Default, Clone)]
#[must_use]
pub struct RepairNoteLinksReport {
    /// Number of candidate notes examined (those with a wikilink in the body or an edge).
    pub notes_scanned: usize,
    /// Number of stale edges removed (or that would be removed, in dry-run).
    pub edges_deleted: usize,
    /// Number of missing edges inserted (or that would be inserted, in dry-run).
    pub edges_added: usize,
    /// Number of notes already consistent (empty diff — nothing to do).
    pub notes_unchanged: usize,
    /// `true` when the run was a dry-run.
    pub dry_run: bool,
}

/// Returns the live notes of this vault that are worth reconciling: those whose body
/// carries at least one `[[` wikilink, **or** which already have at least one outgoing
/// edge (so stale edges left after a body lost all its links are still caught).
///
/// Notes with neither a wikilink nor an edge have nothing to reconcile and are skipped.
///
/// # Errors
///
/// The database is unreachable, or the query fails.
fn notes_to_reconcile(
    conn: &rusqlite::Connection,
    vault_id: &str,
    limit: Option<usize>,
) -> Result<Vec<(String, String)>> {
    let limit_clause = limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    let query = format!(
        "SELECT n.id, n.body_text \
         FROM notes n \
         WHERE n.vault_id = ?1 \
           AND n.status = 'live' \
           AND ( n.body_text LIKE '%[[%' \
                 OR EXISTS ( SELECT 1 FROM note_links l \
                             WHERE l.src_note_id = n.id AND l.vault_id = n.vault_id ) ) \
         ORDER BY n.created ASC \
         {limit_clause}"
    );

    let mut stmt = conn
        .prepare(&query)
        .context("preparing SELECT notes to reconcile")?;

    let rows: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![vault_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("executing SELECT notes to reconcile")?
        .collect::<std::result::Result<_, _>>()
        .context("collecting notes to reconcile")?;

    drop(stmt);
    Ok(rows)
}

/// Resolves the desired outgoing edges of one note from its body, using the same lookup
/// logic the worker applies at write time (reserved nodes → synthetic edge, `section:ULID`
/// → id lookup, free title → H1 lookup). Returns a de-duplicated set of `(src, dst)`.
///
/// Mirrors [`crate::backfill_note_links`]'s closures exactly — the two commands MUST
/// resolve identically, otherwise the reconciler would fight the writer.
fn desired_edges(
    conn: &rusqlite::Connection,
    vault_id: &str,
    note_id: &str,
    body: &str,
) -> BTreeSet<(String, String)> {
    gradatum_curator::wikilinks_sync::resolve_wikilinks_sync(
        vault_id,
        note_id,
        body,
        // id_lookup_fn: a live note with this ULID exists?
        |vlt, ulid| {
            conn.query_row(
                "SELECT id FROM notes \
                 WHERE vault_id = ?1 \
                   AND id = ?2 \
                   AND id NOT LIKE '__sentinel__%' \
                   AND status = 'live'",
                rusqlite::params![vlt, ulid],
                |row| row.get::<_, String>(0),
            )
            .ok()
        },
        // title_lookup_fn: H1 resolution, parity with gradatum_index::Index::title_lookup.
        |vlt, title| {
            let escaped = escape_like_pattern(title);
            let pattern = format!("# {escaped}\n%");
            let pattern_no_lf = format!("# {escaped}");
            conn.query_row(
                "SELECT id FROM notes \
                 WHERE vault_id = ?1 \
                   AND id NOT LIKE '__sentinel__%' \
                   AND status = 'live' \
                   AND (body_text LIKE ?2 ESCAPE '\\' OR body_text = ?3) \
                 ORDER BY created DESC \
                 LIMIT 1",
                rusqlite::params![vlt, pattern, pattern_no_lf],
                |row| row.get::<_, String>(0),
            )
            .ok()
        },
    )
    .into_iter()
    .collect()
}

/// Reads the current outgoing edges of one note from `note_links`, scoped by vault.
///
/// # Errors
///
/// The query fails.
fn current_edges(
    conn: &rusqlite::Connection,
    vault_id: &str,
    src_note_id: &str,
) -> Result<BTreeSet<(String, String)>> {
    let mut stmt = conn
        .prepare(
            "SELECT src_note_id, dst_note_id FROM note_links \
             WHERE src_note_id = ?1 AND vault_id = ?2",
        )
        .context("preparing SELECT current note_links")?;

    let set: BTreeSet<(String, String)> = stmt
        .query_map(rusqlite::params![src_note_id, vault_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("executing SELECT current note_links")?
        .collect::<std::result::Result<_, _>>()
        .context("collecting current note_links")?;

    drop(stmt);
    Ok(set)
}

/// Runs the full `note_links` reconciliation for one tenant.
///
/// In `dry_run` mode the candidate notes are diffed and the would-be changes are printed,
/// but nothing is written.
///
/// # Errors
///
/// The database is unreachable, or a mutation fails.
pub async fn run(args: RepairNoteLinksArgs) -> Result<RepairNoteLinksReport> {
    let db_path = vault_index_path(&args.root);

    if !db_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the server must have started at least once",
            db_path.display()
        );
    }

    tokio::task::spawn_blocking(move || {
        run_repair_sync(&db_path, &args.tenant, args.dry_run, args.limit)
    })
    .await
    .context("spawn_blocking repair_note_links")?
}

/// Synchronous reconciliation implementation; called from `spawn_blocking`.
fn run_repair_sync(
    db_path: &std::path::Path,
    tenant: &str,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<RepairNoteLinksReport> {
    let conn =
        rusqlite::Connection::open(db_path).context("opening index.db for repair-note-links")?;

    // WAL pragma: concurrent read/write with a running gradatum-server.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .context("PRAGMA journal_mode=WAL")?;

    let candidates = notes_to_reconcile(&conn, tenant, limit).context("scan notes to reconcile")?;

    let notes_scanned = candidates.len();
    let mut edges_deleted = 0usize;
    let mut edges_added = 0usize;
    let mut notes_unchanged = 0usize;

    for (note_id, body) in &candidates {
        // Resolution (may run DB lookups) fully completes and drops its statements before
        // the transaction below borrows the connection again.
        let desired = desired_edges(&conn, tenant, note_id, body);
        let current = current_edges(&conn, tenant, note_id)
            .with_context(|| format!("current_edges note_id={note_id}"))?;

        let to_delete: Vec<&(String, String)> = current.difference(&desired).collect();
        let to_add: Vec<&(String, String)> = desired.difference(&current).collect();

        if to_delete.is_empty() && to_add.is_empty() {
            notes_unchanged += 1;
            continue;
        }

        if dry_run {
            for (src, dst) in &to_delete {
                println!("  - DELETE  {src} -> {dst}");
            }
            for (src, dst) in &to_add {
                println!("  + ADD     {src} -> {dst}");
            }
        } else {
            // Per-note atomicity: delete stale + insert missing in one transaction. On
            // failure the note is left untouched; the run is idempotent, so a re-run
            // converges.
            let tx = conn
                .unchecked_transaction()
                .with_context(|| format!("begin tx note_id={note_id}"))?;
            for (src, dst) in &to_delete {
                tx.execute(
                    "DELETE FROM note_links \
                     WHERE src_note_id = ?1 AND dst_note_id = ?2 AND vault_id = ?3",
                    rusqlite::params![src, dst, tenant],
                )
                .with_context(|| format!("DELETE note_links src={src} dst={dst}"))?;
            }
            let now_ms = chrono::Utc::now().timestamp_millis();
            for (src, dst) in &to_add {
                tx.execute(
                    "INSERT OR IGNORE INTO note_links \
                     (src_note_id, dst_note_id, vault_id, created_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![src, dst, tenant, now_ms],
                )
                .with_context(|| format!("INSERT note_links src={src} dst={dst}"))?;
            }
            tx.commit()
                .with_context(|| format!("commit tx note_id={note_id}"))?;
        }

        edges_deleted += to_delete.len();
        edges_added += to_add.len();
    }

    tracing::info!(
        notes_scanned,
        edges_deleted,
        edges_added,
        notes_unchanged,
        dry_run,
        "repair-note-links complete"
    );

    Ok(RepairNoteLinksReport {
        notes_scanned,
        edges_deleted,
        edges_added,
        notes_unchanged,
        dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal schema for the tests (`notes` + `note_links`), mirroring the post-0032 shape.
    fn create_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            // PK composite `(vault_id, id)` — parité avec la migration 0032 (partition par
            // vault). Une PK sur `id` seul interdirait le scénario d'isolation inter-locataires
            // (même ULID dans deux vaults), que le test de non-fuite doit couvrir.
            "CREATE TABLE notes (
                id        TEXT    NOT NULL,
                vault_id  TEXT    NOT NULL,
                status    TEXT    NOT NULL,
                body_text TEXT    NOT NULL DEFAULT '',
                title     TEXT    NOT NULL DEFAULT '',
                created   INTEGER NOT NULL DEFAULT 0,
                updated   INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (vault_id, id)
            );
            CREATE TABLE note_links (
                src_note_id TEXT    NOT NULL,
                dst_note_id TEXT    NOT NULL,
                vault_id    TEXT    NOT NULL,
                created_at  INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (src_note_id, dst_note_id, vault_id)
            );",
        )
    }

    fn insert_note(conn: &rusqlite::Connection, id: &str, vault: &str, body: &str) {
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text) VALUES (?1, ?2, 'live', ?3)",
            rusqlite::params![id, vault, body],
        )
        .expect("insert note");
    }

    fn insert_edge(conn: &rusqlite::Connection, src: &str, dst: &str, vault: &str) {
        conn.execute(
            "INSERT INTO note_links (src_note_id, dst_note_id, vault_id, created_at) \
             VALUES (?1, ?2, ?3, 0)",
            rusqlite::params![src, dst, vault],
        )
        .expect("insert edge");
    }

    fn edge_dsts(conn: &rusqlite::Connection, src: &str, vault: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT dst_note_id FROM note_links \
                 WHERE src_note_id = ?1 AND vault_id = ?2 ORDER BY dst_note_id",
            )
            .expect("prepare");
        let v: Vec<String> = stmt
            .query_map(rusqlite::params![src, vault], |r| r.get::<_, String>(0))
            .expect("query")
            .collect::<std::result::Result<_, _>>()
            .expect("collect");
        v
    }

    fn setup_db_in_layout(root: &std::path::Path) -> rusqlite::Connection {
        let vault_gradatum = root.join("vault").join(".gradatum");
        std::fs::create_dir_all(&vault_gradatum).expect("create vault/.gradatum");
        let db_path = vault_gradatum.join("index.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open DB");
        create_schema(&conn).expect("schema");
        conn
    }

    fn open_db(root: &std::path::Path) -> rusqlite::Connection {
        let db_path = root.join("vault").join(".gradatum").join("index.db");
        rusqlite::Connection::open(&db_path).expect("open DB verif")
    }

    /// A stale edge (present in the table, absent from the body) is removed, and the edge the
    /// current body produces is kept. This is the F-147 accumulation defect being cured.
    #[tokio::test]
    async fn real_run_deletes_stale_edge_and_keeps_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());

        // Body links only [[status:DONE]] now, but the table also carries a stale status:OLD.
        insert_note(&conn, "src", "main", "Voir [[status:DONE]]");
        insert_edge(&conn, "src", "status:DONE", "main");
        insert_edge(&conn, "src", "status:OLD", "main");
        drop(conn);

        let args = RepairNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: false,
            limit: None,
        };
        let report = run(args).await.expect("run");

        assert_eq!(report.edges_deleted, 1, "1 arête périmée supprimée");
        assert_eq!(
            report.edges_added, 0,
            "aucune arête à ajouter (déjà présente)"
        );

        let conn = open_db(dir.path());
        assert_eq!(
            edge_dsts(&conn, "src", "main"),
            vec!["status:DONE".to_string()],
            "seule l'arête du corps courant subsiste"
        );
    }

    /// A note whose body lost all its links has its dangling edges removed.
    #[tokio::test]
    async fn real_run_removes_all_edges_when_body_has_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());

        insert_note(&conn, "src", "main", "Corps sans aucun wikilink.");
        insert_edge(&conn, "src", "status:OLD", "main");
        drop(conn);

        let args = RepairNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: false,
            limit: None,
        };
        let report = run(args).await.expect("run");

        assert_eq!(report.notes_scanned, 1, "note candidate via EXISTS(edge)");
        assert_eq!(report.edges_deleted, 1, "l'arête orpheline est supprimée");
        let conn = open_db(dir.path());
        assert!(
            edge_dsts(&conn, "src", "main").is_empty(),
            "plus aucune arête après reconciliation"
        );
    }

    /// A missing edge (body links a reserved node, table has nothing) is inserted.
    #[tokio::test]
    async fn real_run_adds_missing_edge() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());
        insert_note(&conn, "src", "main", "Voir [[status:DONE]]");
        drop(conn);

        let args = RepairNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: false,
            limit: None,
        };
        let report = run(args).await.expect("run");
        assert_eq!(report.edges_added, 1);
        assert_eq!(report.edges_deleted, 0);
        let conn = open_db(dir.path());
        assert_eq!(
            edge_dsts(&conn, "src", "main"),
            vec!["status:DONE".to_string()]
        );
    }

    /// Dry-run resolves and diffs but writes nothing.
    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());
        insert_note(&conn, "src", "main", "Voir [[status:DONE]]");
        insert_edge(&conn, "src", "status:OLD", "main");
        drop(conn);

        let args = RepairNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: true,
            limit: None,
        };
        let report = run(args).await.expect("run dry");

        assert!(report.dry_run);
        assert_eq!(
            report.edges_deleted, 1,
            "compte l'arête qui SERAIT supprimée"
        );
        assert_eq!(report.edges_added, 1, "compte l'arête qui SERAIT ajoutée");

        // Rien n'a bougé en base.
        let conn = open_db(dir.path());
        let mut dsts = edge_dsts(&conn, "src", "main");
        dsts.sort();
        assert_eq!(
            dsts,
            vec!["status:OLD".to_string()],
            "la base est inchangée en dry-run (status:OLD toujours là, status:DONE pas ajouté)"
        );
    }

    /// Isolation: reconciling `main` does not touch a homonymous note (same id) in another vault.
    #[tokio::test]
    async fn reconcile_is_scoped_by_vault() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());

        // Same id "src" in two vaults; only main's body drops the link.
        insert_note(&conn, "src", "main", "Corps sans wikilink.");
        insert_edge(&conn, "src", "status:X", "main");
        insert_note(&conn, "src", "vault-b", "Voir [[status:X]]");
        insert_edge(&conn, "src", "status:X", "vault-b");
        drop(conn);

        let args = RepairNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: false,
            limit: None,
        };
        let _ = run(args).await.expect("run main");

        let conn = open_db(dir.path());
        assert!(
            edge_dsts(&conn, "src", "main").is_empty(),
            "l'arête de main est supprimée"
        );
        assert_eq!(
            edge_dsts(&conn, "src", "vault-b"),
            vec!["status:X".to_string()],
            "isolation : l'arête homonyme de vault-b reste intacte"
        );
    }

    /// Idempotence: a second pass over a reconciled DB changes nothing.
    #[tokio::test]
    async fn run_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());
        insert_note(&conn, "src", "main", "Voir [[status:DONE]]");
        insert_edge(&conn, "src", "status:OLD", "main");
        drop(conn);

        let make_args = || RepairNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: false,
            limit: None,
        };

        let r1 = run(make_args()).await.expect("run 1");
        assert_eq!(r1.edges_deleted, 1);
        assert_eq!(r1.edges_added, 1);

        let r2 = run(make_args()).await.expect("run 2");
        assert_eq!(r2.edges_deleted, 0, "2e passage : rien à supprimer");
        assert_eq!(r2.edges_added, 0, "2e passage : rien à ajouter");
        assert_eq!(r2.notes_unchanged, 1, "note déjà cohérente");
    }
}
