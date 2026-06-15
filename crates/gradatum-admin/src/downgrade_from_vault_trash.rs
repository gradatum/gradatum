//! Migration tool: downgrades notes from a legacy vault's `.vault-trash/**/*.md`
//! to `status='downgraded'` in Gradatum via a direct SQLite UPDATE.
//!
//! ## Supported vault-trash structure
//!
//! Recursively walks `.vault-trash/` via `walkdir`, supporting all depths
//! observed in production:
//!
//! - 2-level legacy layout: `.vault-trash/<date>/<file>.md`
//! - 4-level dedup layout:  `.vault-trash/<date>/dedup/<section>/<file>.md`
//!
//! The dated directory name (first component relative to `.vault-trash/`) is
//! extracted from the relative path to build the `status_reason`.
//!
//! ## Behaviour
//!
//! Idempotent: skips notes already at `status='downgraded'`.
//! Match heuristic: `substr(body_text, 1, 200)` of the file compared to the database (UTF-8 safe).
//! Dry-run mode and `--limit` are supported for incremental use.
//!
//! ## Usage
//! ```text
//! gradatum-admin downgrade-from-legacy-vault-trash --legacy-vault-path ~/.memory-vault --root /var/lib/gradatum
//! gradatum-admin downgrade-from-legacy-vault-trash --dry-run --limit 50
//! ```

use anyhow::{Context, Result};
use gradatum_core::paths::vault_index_path;
use gradatum_storage::{FileStorage, Storage as _};
use std::path::PathBuf;
use walkdir::WalkDir;

/// Arguments for the `downgrade-from-legacy-vault-trash` sub-command.
#[derive(Debug, Clone)]
pub struct DowngradeFromTrashArgs {
    /// Root directory of the legacy vault (containing `.vault-trash/`).
    pub legacy_vault_path: PathBuf,
    /// Gradatum root directory (e.g. `/var/lib/gradatum`).
    pub gradatum_root: PathBuf,
    /// Dry-run mode: logs planned actions without writing to the database.
    pub dry_run: bool,
    /// Maximum number of notes to downgrade (real or dry-run). `None` = unlimited.
    pub limit: Option<usize>,
}

/// Statistics returned by [`run`].
#[derive(Debug, Default)]
pub struct DowngradeStats {
    /// Number of `.md` files scanned in `.vault-trash/`.
    pub trash_files_scanned: usize,
    /// Number of files matched in `notes` (any status).
    pub matched_in_gradatum: usize,
    /// Number of notes already at `status='downgraded'` (idempotent skip).
    pub already_downgraded: usize,
    /// Number of notes actually downgraded (or counted in dry-run).
    pub downgraded: usize,
    /// Number of files with no match in Gradatum.
    pub not_matched: usize,
}

/// Strips the YAML frontmatter `---\n...\n---\n` if present.
///
/// Returns the body without frontmatter. If the format is not recognised,
/// returns the input text unchanged.
fn strip_frontmatter(body: &str) -> &str {
    if !body.starts_with("---\n") {
        return body;
    }
    // Find the closing delimiter "\n---\n" after the opening "---\n".
    if let Some(end) = body[4..].find("\n---\n") {
        // end is relative to body[4..], so skip "---\n" (4) + content (end) + "\n---\n" (5).
        return &body[4 + end + 5..];
    }
    body
}

/// Migrates notes from a legacy vault's `.vault-trash` to Gradatum via `status='downgraded'`.
///
/// ## Behaviour
/// - Recursively walks `.vault-trash/**/*.md` via `walkdir` (arbitrary depth).
///   Supports both the 2-level legacy layout and the 4-level dedup layout.
/// - For each `.md`, strips YAML frontmatter and takes the first 200 characters (UTF-8 safe).
/// - Looks up the match in `notes.body_text` via `substr(body_text, 1, 200)`.
/// - On match where `status != 'downgraded'` → atomic UPDATE (status + status_reason + timestamps).
/// - Skips notes already downgraded (idempotent).
/// - `--limit N` stops after N downgrades are counted (real or dry-run).
///
/// ## Side effects
/// - Writes directly to `vault/.gradatum/index.db` (SQL UPDATE).
/// - Dry-run: logs planned actions to stderr; no writes.
/// - Logs a final summary to stderr.
pub async fn run(args: DowngradeFromTrashArgs) -> Result<DowngradeStats> {
    let trash_dir = args.legacy_vault_path.join(".vault-trash");
    if !trash_dir.exists() {
        // Idempotent: no trash dir = nothing to migrate = OK.
        // Unlike a missing index.db (fatal infrastructure error), a missing
        // .vault-trash directory is a valid and expected state on a clean vault.
        let stats = DowngradeStats::default();
        eprintln!(
            "info: .vault-trash absent ({}) — rien à migrer (idempotent)",
            trash_dir.display()
        );
        eprintln!(
            "downgrade-from-legacy-vault-trash: scanned={} matched={} already_downgraded={} downgraded={} not_matched={}",
            stats.trash_files_scanned,
            stats.matched_in_gradatum,
            stats.already_downgraded,
            stats.downgraded,
            stats.not_matched
        );
        return Ok(stats);
    }

    // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
    let index_path = vault_index_path(&args.gradatum_root);
    if !index_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le worker doit avoir démarré au moins une fois",
            index_path.display()
        );
    }

    // OpenDAL FileStorage rooted at legacy_vault_path — used to read trash .md files.
    // legacy_vault_path is an external source (legacy vault), not the Gradatum vault.
    // This FileStorage instance is ephemeral and scoped to this migration operation.
    let trash_storage = FileStorage::new(&args.legacy_vault_path).with_context(|| {
        format!(
            "FileStorage init legacy_vault_path {}",
            args.legacy_vault_path.display()
        )
    })?;

    let mut stats = DowngradeStats::default();
    let conn = rusqlite::Connection::open(&index_path).context("ouverture index.db")?;

    // Prepare the query once before the loop: SQL preparation is expensive
    // (parsing + compilation). Calling `conn.prepare()` inside the loop would
    // recompile the query on every iteration — O(N) unnecessary preparations.
    // Hoisted here: one preparation, the Statement is reused.
    let mut stmt = conn
        .prepare("SELECT id, status FROM notes WHERE substr(body_text, 1, 200) = ?1 LIMIT 1")
        .context("préparation requête match")?;

    // Recursive walk of .vault-trash/**/*.md — supports all depths:
    //   - 2-level legacy: .vault-trash/<date>/<file>.md         (min_depth=2)
    //   - 4-level dedup:  .vault-trash/<date>/dedup/<section>/<file>.md
    //
    // min_depth=2: skips .vault-trash/ itself (depth=0) and dated directories
    //   (depth=1 = no .md files expected at that level).
    // max_depth=10: safety guard, well above the observed maximum depth.
    for entry in WalkDir::new(&trash_dir)
        .min_depth(2)
        .max_depth(10)
        .into_iter()
        .filter_map(|e| {
            // Skip unreadable entries (permissions) without interrupting the walk.
            e.map_err(|err| {
                eprintln!("[WARN] entrée inaccessible dans .vault-trash : {err}");
            })
            .ok()
        })
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        // Limit guard: stop globally once the downgrade limit is reached.
        if let Some(limit) = args.limit {
            if stats.downgraded >= limit {
                break;
            }
        }

        // Extract the dated directory name = first component relative to trash_dir.
        // Examples:
        //   .vault-trash/2026-05-09/note.md             → "2026-05-09"
        //   .vault-trash/2026-05-09/dedup/ref/note.md   → "2026-05-09"
        let date_dir_name = path
            .strip_prefix(&trash_dir)
            .unwrap_or(path)
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());

        // Relative path from legacy_vault_path — required by the Storage trait.
        // path is absolute (walkdir); strip_prefix recovers the relative portion.
        let rel_path = match path.strip_prefix(&args.legacy_vault_path) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => {
                eprintln!("[WARN] chemin hors legacy_vault_path : {}", path.display());
                continue;
            }
        };

        let body_bytes = match trash_storage.read(&rel_path).await {
            Ok(b) => b,
            Err(e) => {
                // `trash_files_scanned` is incremented AFTER a successful read.
                // Incrementing before a failed read and then decrementing would risk
                // underflow (usize wraps on overflow in release mode if the first
                // file encountered is unreadable).
                eprintln!("[WARN] lecture impossible {} : {e}", path.display());
                continue; // compteur PAS incrémenté pour les fichiers illisibles
            }
        };
        // Convert bytes → UTF-8 String (Markdown notes are always UTF-8).
        // Lossy conversion for resilience against potentially corrupted bytes.
        let body = String::from_utf8_lossy(&body_bytes).into_owned();

        // Increment AFTER successful read (safe, no underflow risk).
        stats.trash_files_scanned += 1;

        let body_clean = strip_frontmatter(&body);
        // First 200 chars (UTF-8 safe via chars().take()).
        let needle: String = body_clean.chars().take(200).collect();

        let row: Option<(String, String)> = stmt
            .query_row(rusqlite::params![needle], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .ok();

        match row {
            Some((id, status)) => {
                stats.matched_in_gradatum += 1;

                if status == "downgraded" {
                    stats.already_downgraded += 1;
                    continue;
                }

                if args.dry_run {
                    eprintln!(
                        "[DRY-RUN] would downgrade note_id={id} (file={})",
                        path.display()
                    );
                    stats.downgraded += 1;
                    continue;
                }

                // Atomic UPDATE: status + status_reason + timestamps.
                let now = chrono::Utc::now().timestamp_millis();
                let reason = format!("migrated from legacy-vault .vault-trash/{date_dir_name}/");

                conn.execute(
                    "UPDATE notes \
                     SET status = 'downgraded', \
                         status_reason = ?2, \
                         status_changed = ?3, \
                         updated = ?3 \
                     WHERE id = ?1",
                    rusqlite::params![id, reason, now],
                )
                .context("UPDATE downgrade note")?;

                stats.downgraded += 1;
            }
            None => {
                stats.not_matched += 1;
            }
        }
    }

    eprintln!(
        "downgrade-from-legacy-vault-trash: scanned={} matched={} already_downgraded={} downgraded={} not_matched={}",
        stats.trash_files_scanned,
        stats.matched_in_gradatum,
        stats.already_downgraded,
        stats.downgraded,
        stats.not_matched
    );

    Ok(stats)
}
