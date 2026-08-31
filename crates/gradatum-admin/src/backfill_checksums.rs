//! `gradatum-admin backfill-checksums` sub-command (F-174 geste 2).
//!
//! Retro-fills the `file_checksums` drift footprints for note `.md` files that are on disk
//! but have **no** entry yet — the notes written before F-165 wired the footprint into the
//! write funnel. Without this, `scan_phase_a` only sees what was written after that wiring:
//! it would report "no drift" on a vault it barely covers (critère 1 of the card).
//!
//! ## Footprint only — never a rewrite
//!
//! Unlike `reindex-orphans` (which re-runs the full write funnel, bumping `updated` and
//! re-embedding), this command writes **only** the `file_checksums` row: it hashes the bytes
//! already on disk and upserts the footprint. It does not touch the `.md`, the index note
//! row, the embeddings, or the queue. Re-serialising thousands of stable notes to gain a
//! checksum would be a needless mass mutation.
//!
//! The footprint is built by the single shared helper
//! [`gradatum_index::drift::build_note_checksum_entry`], so the retro-filled checksum is
//! byte-for-byte what `scan_phase_a` compares against — no spurious drift.
//!
//! ## Offline operation
//!
//! `gradatum-server` is the sole writer of `index.db`; a second concurrent writer would
//! race on the lock and report a partial success. Run this with the server stopped, and
//! take a backup first. `--dry-run` previews without writing.
//!
//! ## Idempotence
//!
//! Candidates are the set-difference `on-disk note .md − file_checksums entries`, recomputed
//! each run. A footprinted file leaves the set the moment its row exists, so a second run
//! finds zero candidates.
//!
//! ## Usage
//! ```text
//! gradatum-admin backfill-checksums --root /var/lib/gradatum --dry-run
//! gradatum-admin backfill-checksums --root /var/lib/gradatum
//! gradatum-admin backfill-checksums --root /var/lib/gradatum --tenant code-gradatum --limit 100
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gradatum_core::paths::vault_index_path;
use gradatum_index::SqliteIndex;
use gradatum_index::drift::build_note_checksum_entry;
use ulid::Ulid;
use walkdir::WalkDir;

use crate::backfill_embeddings::guard_tenant_scope;

/// Arguments for the `backfill-checksums` sub-command.
#[derive(Debug, Clone)]
pub struct BackfillChecksumsArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`) — holds `vault/`.
    pub root: PathBuf,
    /// Target tenant / `vault_id` (default: `"main"`).
    pub tenant: Option<String>,
    /// Maximum number of files to footprint; unlimited when absent.
    pub limit: Option<usize>,
    /// Preview actions without writing anything.
    pub dry_run: bool,
}

/// Report of a `backfill-checksums` run.
#[derive(Debug, Default, Clone)]
#[must_use = "the report states how many footprints were written and how many files were unreadable"]
pub struct BackfillChecksumsReport {
    /// Note `.md` on disk with no `file_checksums` entry (after `--limit`).
    pub candidates_found: usize,
    /// Footprints now present in `file_checksums`. **Read back from the table** in a real
    /// run (never the loop counter); in dry-run this is the count that *would* be written.
    pub backfilled: usize,
    /// Files skipped because their bytes could not be read (surfaced, not silenced).
    pub skipped_unreadable: usize,
    /// `true` when the run was a dry-run.
    pub dry_run: bool,
}

/// One candidate discovered on disk: its relative path under the vault directory.
#[derive(Debug, Clone)]
struct Candidate {
    /// Path relative to `<root>/vault`, e.g. `main/01ID.md`.
    relative_path: String,
}

/// Entry point. Scans for note `.md` lacking a footprint, guards the volume, then (unless
/// dry-run) hashes each on-disk file and upserts its `file_checksums` row.
///
/// # Errors
/// - `index.db` absent → descriptive error.
/// - The volume guard refuses an unbounded mass operation (see `guard_tenant_scope`).
/// - A filesystem or SQLite error during the scan or upsert.
pub async fn run(args: BackfillChecksumsArgs) -> Result<BackfillChecksumsReport> {
    let index_path = vault_index_path(&args.root);
    if !index_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the server must have started at least once",
            index_path.display()
        );
    }

    let tenant = args.tenant.as_deref().unwrap_or("main").to_string();
    let vault_dir = args.root.join("vault");

    let footprinted = load_footprinted_paths(&index_path).context("loading file_checksums")?;
    let candidates = scan_unfootprinted(&vault_dir, &tenant, &footprinted, args.limit)
        .context("scanning disk for un-footprinted notes")?;

    if candidates.is_empty() {
        eprintln!(
            "backfill-checksums: 0 candidates — file_checksums already covers all .md files (tenant='{tenant}')"
        );
        return Ok(BackfillChecksumsReport {
            dry_run: args.dry_run,
            ..Default::default()
        });
    }

    let candidates_found = candidates.len();

    // Garde-fou AVANT toute écriture — MÊME garde que backfill-embeddings (réutilisée).
    guard_tenant_scope(&tenant, candidates_found, args.limit)?;

    if args.dry_run {
        // Pré-vol : compter ceux dont les octets sont lisibles, sans ouvrir l'index en écriture.
        let mut would = 0usize;
        let mut unreadable = 0usize;
        for cand in &candidates {
            if std::fs::read(vault_dir.join(&cand.relative_path)).is_ok() {
                would += 1;
            } else {
                unreadable += 1;
            }
        }
        eprintln!(
            "backfill-checksums [DRY-RUN]: {candidates_found} candidate(s) — {would} footprint(s) to write, \
             {unreadable} unreadable (tenant='{tenant}')"
        );
        return Ok(BackfillChecksumsReport {
            candidates_found,
            backfilled: would,
            skipped_unreadable: unreadable,
            dry_run: true,
        });
    }

    eprintln!(
        "backfill-checksums: {candidates_found} candidate(s) (tenant='{tenant}') — writing footprints..."
    );

    let idx = SqliteIndex::open(&index_path)
        .await
        .map_err(|e| anyhow::anyhow!("opening index.db {}: {e}", index_path.display()))?;

    let now_secs = chrono::Utc::now().timestamp();
    let mut report = BackfillChecksumsReport {
        candidates_found,
        dry_run: false,
        ..Default::default()
    };
    let mut written_paths: Vec<String> = Vec::with_capacity(candidates_found);

    for cand in &candidates {
        let abs = vault_dir.join(&cand.relative_path);
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    path = %cand.relative_path,
                    err = %e,
                    "backfill-checksums: .md unreadable — skipped"
                );
                report.skipped_unreadable += 1;
                continue;
            }
        };
        let entry = build_note_checksum_entry(cand.relative_path.clone(), &bytes, now_secs);
        idx.upsert_file_checksum(&entry)
            .await
            .map_err(|e| anyhow::anyhow!("upsert footprint for {}: {e}", cand.relative_path))?;
        written_paths.push(cand.relative_path.clone());
    }

    // ── Compteur relu DANS la table (jamais written_paths.len()) ──────────────────
    // Une écriture restée sans effet doit se voir : le compte vient de file_checksums.
    report.backfilled = count_present_footprints(&index_path, &written_paths)
        .context("re-reading footprints from index.db")?;

    tracing::info!(
        candidates_found = report.candidates_found,
        backfilled = report.backfilled,
        skipped_unreadable = report.skipped_unreadable,
        "backfill-checksums complete"
    );

    Ok(report)
}

/// Scans the tenant directory for note `.md` files that have no `file_checksums` entry.
///
/// Hidden directories (`.history/`, `.archive/`, `.gradatum/`) are pruned and symlinks are
/// never followed (`follow_links(false)` — path-traversal safety, ADN 5). Files whose stem
/// is not a ULID are ignored (not notes). Candidates are sorted by relative path for
/// determinism, then `--limit` is applied.
fn scan_unfootprinted(
    vault_dir: &Path,
    tenant: &str,
    footprinted: &HashSet<String>,
    limit: Option<usize>,
) -> Result<Vec<Candidate>> {
    let tenant_dir = vault_dir.join(tenant);
    if !tenant_dir.exists() {
        return Ok(Vec::new());
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let walker = WalkDir::new(&tenant_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_hidden(e));
    for entry in walker {
        let entry = entry.context("walking the vault directory")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if Ulid::from_string(stem).is_err() {
            continue; // nom non-ULID → pas une note
        }
        let rel = path
            .strip_prefix(vault_dir)
            .map_err(|e| anyhow::anyhow!("relativising {}: {e}", path.display()))?
            .to_string_lossy()
            .to_string();
        if footprinted.contains(&rel) {
            continue; // déjà une empreinte → pas un candidat
        }
        candidates.push(Candidate { relative_path: rel });
    }

    candidates.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    if let Some(n) = limit {
        candidates.truncate(n);
    }
    Ok(candidates)
}

/// `true` when the walked entry is a hidden file or directory (name starts with `.`).
fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|n| n.starts_with('.'))
}

/// Loads the set of `relative_path` values already present in `file_checksums`, read-only.
fn load_footprinted_paths(index_path: &Path) -> Result<HashSet<String>> {
    let conn = rusqlite::Connection::open_with_flags(
        index_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .context("opening index.db read-only")?;
    let mut stmt = conn
        .prepare("SELECT relative_path FROM file_checksums")
        .context("preparing file_checksums query")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("executing file_checksums query")?;
    let mut set = HashSet::new();
    for row in rows {
        set.insert(row.context("reading relative_path")?);
    }
    Ok(set)
}

/// Counts how many of `paths` now have a `file_checksums` row, read straight from the table.
///
/// This is the backfilled counter — never the loop's own tally. An upsert that silently
/// produced no row is therefore visible.
fn count_present_footprints(index_path: &Path, paths: &[String]) -> Result<usize> {
    if paths.is_empty() {
        return Ok(0);
    }
    let conn = rusqlite::Connection::open_with_flags(
        index_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .context("opening index.db read-only for count")?;
    let mut stmt = conn
        .prepare("SELECT 1 FROM file_checksums WHERE relative_path = ?1")
        .context("preparing footprint count query")?;
    let mut present = 0usize;
    for p in paths {
        if stmt
            .exists(rusqlite::params![p])
            .context("executing footprint count query")?
        {
            present += 1;
        }
    }
    Ok(present)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    /// Writes a note `.md` on disk (bypassing the funnel).
    fn write_md(vault_dir: &Path, rel: &str) {
        let path = vault_dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "---\nvault_id: main\n---\n\n# N\n").unwrap();
    }

    // ── scan : candidat = .md sans empreinte, exclusions cachées/non-ULID/déjà-suivi ─
    #[test]
    fn scan_flags_unfootprinted_notes_and_excludes_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_dir = tmp.path().join("vault");

        let bare = Ulid::generate().to_string();
        let footprinted_id = Ulid::generate().to_string();
        let archived = Ulid::generate().to_string();

        write_md(&vault_dir, &format!("main/{bare}.md"));
        write_md(&vault_dir, &format!("main/{footprinted_id}.md"));
        // Sous .archive/ : stem ULID mais segment caché → jamais candidat.
        write_md(&vault_dir, &format!("main/.archive/{archived}.md"));
        // Fichier non-ULID → jamais une note.
        std::fs::write(vault_dir.join("main/README.md"), "# r\n").unwrap();

        // Une seule empreinte connue : `footprinted_id`.
        let mut footprinted = HashSet::new();
        footprinted.insert(format!("main/{footprinted_id}.md"));

        let got: HashSet<String> = scan_unfootprinted(&vault_dir, "main", &footprinted, None)
            .unwrap()
            .into_iter()
            .map(|c| c.relative_path)
            .collect();

        assert!(
            got.contains(&format!("main/{bare}.md")),
            "la note sans empreinte est un candidat"
        );
        assert!(
            !got.contains(&format!("main/{footprinted_id}.md")),
            "une note déjà suivie n'est pas un candidat"
        );
        assert!(
            got.iter().all(|p| !p.contains("/.archive/")),
            "une note archivée (segment caché) n'est jamais candidate"
        );
        assert!(
            !got.iter().any(|p| p.ends_with("README.md")),
            "un fichier non-ULID n'est jamais candidat"
        );
    }

    // ── end-to-end : empreinte écrite, compteur relu en table, idempotence ───────
    #[tokio::test]
    async fn backfill_writes_footprint_counted_from_table_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vault_dir = root.join("vault");
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .unwrap();
        std::fs::create_dir_all(vault_dir.join("main")).unwrap();

        let id = Ulid::generate().to_string();
        write_md(&vault_dir, &format!("main/{id}.md"));

        let args = || BackfillChecksumsArgs {
            root: root.to_path_buf(),
            tenant: None,
            limit: None,
            dry_run: false,
        };
        let report = run(args()).await.unwrap();
        assert_eq!(report.candidates_found, 1);
        assert_eq!(report.backfilled, 1, "l'empreinte est relue dans la table");

        // L'entrée existe bien dans file_checksums.
        let index_path = vault_index_path(root);
        let rows: i64 = {
            let conn = rusqlite::Connection::open(&index_path).unwrap();
            conn.query_row(
                "SELECT count(*) FROM file_checksums WHERE relative_path = ?1",
                rusqlite::params![format!("main/{id}.md")],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(rows, 1, "une empreinte pour la note");

        // Idempotence : 2e run → 0 candidat.
        let second = run(args()).await.unwrap();
        assert_eq!(second.candidates_found, 0, "plus aucun candidat au 2e run");
        assert_eq!(second.backfilled, 0);
    }

    // ── dry-run : rien écrit ─────────────────────────────────────────────────────
    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vault_dir = root.join("vault");
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .unwrap();
        std::fs::create_dir_all(vault_dir.join("main")).unwrap();

        let id = Ulid::generate().to_string();
        write_md(&vault_dir, &format!("main/{id}.md"));

        let report = run(BackfillChecksumsArgs {
            root: root.to_path_buf(),
            tenant: None,
            limit: None,
            dry_run: true,
        })
        .await
        .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.candidates_found, 1);
        assert_eq!(report.backfilled, 1, "dry-run compte ce qui SERAIT écrit");

        // Rien en base.
        let index_path = vault_index_path(root);
        let rows: i64 = {
            let conn = rusqlite::Connection::open(&index_path).unwrap();
            conn.query_row("SELECT count(*) FROM file_checksums", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(rows, 0, "dry-run n'écrit aucune empreinte");
    }

    // ── garde : le compteur relu détecte une empreinte fantôme ───────────────────
    // Non-régression du contrat "compté depuis la table" : une entrée que le walk a
    // vue mais qui n'a pas de ligne n'est jamais comptée.
    #[test]
    fn count_present_footprints_reads_the_table() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("index.db");
        let conn = rusqlite::Connection::open(&index_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE file_checksums (relative_path TEXT PRIMARY KEY);
             INSERT INTO file_checksums (relative_path) VALUES ('main/present.md');",
        )
        .unwrap();
        drop(conn);

        let n = count_present_footprints(
            &index_path,
            &["main/present.md".to_string(), "main/absent.md".to_string()],
        )
        .unwrap();
        assert_eq!(n, 1, "seule l'empreinte réellement en table est comptée");
    }
}
