//! Sub-command `gradatum-admin code ingest`.
//!
//! Code-ingest pipeline: git repository → tree-sitter parse → index-only derived notes.
//!
//! ## Usage
//! ```text
//! gradatum-admin code ingest <repo_path> --vault code-<project> --root /var/lib/gradatum
//! gradatum-admin code ingest <repo_path> --vault code-gradatum --rebuild
//! ```
//!
//! ## Index-only principle
//!
//! Derived notes live exclusively in SQLite (logical `vault_id`).
//! No Markdown file is created. Provenance = `"derived:tree-sitter"`.
//!
//! ## Idempotence
//!
//! If a file's `content_hash_source` matches the value stored in `code_freshness`,
//! the file is **skipped** (0 writes). A second ingest with no changes produces
//! 0 writes.
//!
//! ## Deletion propagation
//!
//! `source_path` entries present in `code_freshness` but absent from `git ls-files`
//! have their notes deleted.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use gradatum_index::SqliteIndex;
use gradatum_ingest::{build_derived_notes, content_hash_source, parse_rust_file};

/// Report returned by an ingest run.
#[derive(Debug, Default)]
pub struct IngestReport {
    /// Number of files processed (`git ls-files` output, `.rs` extension).
    pub files_total: usize,
    /// Number of files skipped (hash unchanged — idempotent).
    pub files_skipped: usize,
    /// Number of files ingested (hash changed or new).
    pub files_ingested: usize,
    /// Total number of notes inserted.
    pub notes_inserted: usize,
    /// Number of files deleted (absent from `git ls-files`).
    pub files_deleted: usize,
    /// Ingest duration in milliseconds.
    pub duration_ms: u64,
}

/// Ingestion visibility mode.
///
/// Controls which Rust items are extracted during ingest:
/// - `Pub`: only public items (`pub`, `pub(crate)`, etc.) are indexed.
///   Default behaviour; preserves the visible API surface.
/// - `All`: all items are indexed, including private items.
///   Useful for indexing the internal implementation of a crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IngestVisibility {
    /// Index only public items (default behaviour).
    #[default]
    Pub,
    /// Index all items, including private items.
    All,
}

impl IngestVisibility {
    /// Returns the string representation for database persistence.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pub => "pub",
            Self::All => "all",
        }
    }

    /// Parses from the string stored in the database.
    ///
    /// Unknown value → falls back to `Pub` (backward-compatible; accuracy over coverage).
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "all" => Self::All,
            _ => Self::Pub,
        }
    }

    /// Returns `true` if the mode includes private items.
    pub fn include_private(self) -> bool {
        self == Self::All
    }
}

/// Arguments for the `code ingest` command.
#[derive(Debug, Clone, Default)]
pub struct CodeIngestArgs {
    /// Path to the git repository to ingest.
    pub repo_path: PathBuf,
    /// Target logical vault (e.g. `"code-gradatum"`).
    pub vault_id: String,
    /// Path to the Gradatum index (e.g. `/var/lib/gradatum/vault/.gradatum/index.db`).
    pub index_path: PathBuf,
    /// Forces a full rebuild (drop + re-ingest).
    pub rebuild: bool,
    /// Ingestion visibility mode (default: `Pub`).
    ///
    /// Use `..Default::default()` in struct constructors to keep the default mode
    /// without modifying existing call sites.
    pub visibility: IngestVisibility,
}

/// Ingests a git repository into the logical vault.
///
/// ## Workflow
///
/// 1. `git ls-files` (HEAD) → list of versioned `.rs` files.
/// 2. For each file: compute `content_hash_source`; if unchanged → skip.
/// 3. tree-sitter parse → `DerivedSymbol` → `DerivedNote`.
/// 4. `write_note_derived_batch` (atomic transaction per file).
/// 5. Deletion propagation: indexed `source_path` absent from `git ls-files` → delete notes.
///
/// ## Cross-file atomicity
///
/// **Per-file** atomicity is guaranteed by `write_note_derived_batch`
/// (SQL `BEGIN IMMEDIATE / COMMIT / ROLLBACK`). However, **cross-file** atomicity
/// (full run) is not: a crash mid-run would leave the index in a partial state
/// with no detectable signal.
///
/// A marker file `.ingest-incomplete-<vault_id>` is placed in the same directory
/// as `index_path`:
/// - **Placed BEFORE** the first write.
/// - **Removed AFTER** the deletion phase (end of run).
/// - If present at startup: logs a warning and falls back to `rebuild = true`
///   to restore a consistent state.
///
/// `run_update` detects this marker: if present, it refuses to perform a partial
/// diff and calls `run_ingest(rebuild=true)`.
///
/// ## Errors
///
/// Returns an error if:
/// - `repo_path` is not a valid git repository.
/// - `index_path` does not exist.
/// - `git ls-files` fails.
///
/// Unparseable Rust files are silently skipped (accuracy over coverage).
pub async fn run_ingest(args: CodeIngestArgs) -> Result<IngestReport> {
    let start = std::time::Instant::now();

    if !args.index_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le worker doit avoir démarré au moins une fois",
            args.index_path.display()
        );
    }

    let index = SqliteIndex::open(&args.index_path)
        .await
        .with_context(|| format!("ouverture index.db : {}", args.index_path.display()))?;

    // ── Cross-file atomicity marker ───────────────────────────────────────────
    //
    // The `.ingest-incomplete-<vault>` marker signals that a run is in progress (or
    // was interrupted). It is placed BEFORE the first write and removed AFTER the
    // last. Presence at startup → previous run interrupted → log warning +
    // forced rebuild to restore a consistent state.
    //
    // Implementation choice: marker file (vs. encompassing transaction or atomic swap).
    // - Encompassing transaction: SQLite locks the whole DB for the duration of the
    //   run, blocking concurrent server reads (unacceptable).
    // - Atomic swap: complex, requires two index copies (too costly).
    // - **Marker file**: lightweight, robust against OOM/SIGKILL, detectable at startup.
    //
    // Note : le marqueur est nommé par vault_id (sanitisé) pour que deux ingests
    // concurrents sur des vaults différents ne s'interfèrent pas.
    let safe_vault = args
        .vault_id
        .replace(|c: char| !c.is_alphanumeric() && c != '-', "_");
    let marker_path = args
        .index_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!(".ingest-incomplete-{safe_vault}"));

    // Rebuild required if: (a) explicit --rebuild flag, OR (b) incomplete-run marker present.
    let need_rebuild = args.rebuild || marker_path.exists();
    if need_rebuild {
        if marker_path.exists() {
            tracing::warn!(
                vault_id = %args.vault_id,
                marker = %marker_path.display(),
                "marqueur run incomplet détecté — run précédent interrompu. Forçage rebuild."
            );
        }

        // The marker MUST be placed BEFORE any destructive mutation.
        //
        // `delete_vault_from_index` is a destructive, irreversible operation.
        // A crash between the drop and placing the marker would leave the vault
        // emptied WITHOUT a marker: the next `run_update` would see an empty index
        // and perform a partial diff → permanent silent drift.
        //
        // Required order: marker_write → delete_vault → (rest of run) → marker_remove.
        // The marker is absent in the non-rebuild case (no drop); it is placed again
        // below before the first write of Phase 1 to cover the non-rebuild case too.
        // In rebuild mode this placement is redundant but idempotent
        // (`O_CREAT|O_TRUNC` on an existing file is a net no-op).
        std::fs::write(&marker_path, b"").with_context(|| {
            format!(
                "pose marqueur avant drop rebuild : {}",
                marker_path.display()
            )
        })?;

        let deleted = index
            .delete_vault_from_index(&args.vault_id)
            .await
            .with_context(|| format!("delete_vault_from_index({})", args.vault_id))?;
        tracing::info!(
            vault_id = %args.vault_id,
            deleted,
            rebuild_explicit = args.rebuild,
            rebuild_forced = marker_path.exists(),
            "rebuild : vault droppé"
        );
    }

    // Store the absolute repo path and visibility mode for:
    //   1. server-side drift detection — to locate source files.
    //   2. `run_update` — to re-read the mode and re-ingest changed files consistently.
    // Canonicalized so the server (different cwd) can resolve source files.
    let repo_abs = std::fs::canonicalize(&args.repo_path)
        .with_context(|| format!("canonicalize repo_path : {}", args.repo_path.display()))?;
    index
        .set_code_vault_repo_path_with_visibility(
            &args.vault_id,
            &repo_abs.to_string_lossy(),
            args.visibility.as_str(),
        )
        .await
        .with_context(|| {
            format!(
                "set_code_vault_repo_path_with_visibility({})",
                args.vault_id
            )
        })?;

    // HEAD sha for storage in code_freshness.
    let head_sha = git_head_sha(&args.repo_path)?;

    // Retrieve versioned files from the repository (.rs only).
    let git_files = git_ls_files(&args.repo_path)?;

    // Retrieve source_paths already indexed for this vault.
    let indexed_paths = index
        .get_code_freshness_map(&args.vault_id)
        .await
        .with_context(|| format!("get_code_freshness_map({})", args.vault_id))?;

    let mut report = IngestReport::default();

    // Place (or re-place) the marker before Phase 1.
    //
    // Non-rebuild mode: first placement (covers the first write of Phase 1).
    // Rebuild mode: idempotent re-placement (`O_CREAT|O_TRUNC`). The marker was already
    // placed before `delete_vault_from_index` (see above); it is re-placed here
    // to consolidate the visible placement point and ensure it is present before
    // any Phase 1 write, even if the rebuild block evolves in the future.
    std::fs::write(&marker_path, b"")
        .with_context(|| format!("pose marqueur ingest incomplet : {}", marker_path.display()))?;

    // ── Phase 1: process git files ───────────────────────────────────────────

    for relative_path in &git_files {
        if !relative_path.ends_with(".rs") {
            continue;
        }
        report.files_total += 1;

        // Defense-in-depth: reject any absolute path returned by git.
        // `git ls-files -z` never returns absolute paths, but `PathBuf::join`
        // on an absolute path silently ignores the repo prefix and resolves
        // outside the expected sandbox. Two redundant guards for clarity:
        //   1. `Path::new(p).is_absolute()` → true for `/foo` or `C:\foo`
        //   2. `starts_with('/')` → redundant on Unix, explicit for readability
        if std::path::Path::new(relative_path.as_str()).is_absolute()
            || relative_path.starts_with('/')
        {
            tracing::warn!(
                path = %relative_path,
                "git ls-files a retourné un path absolu inattendu — skip défensif"
            );
            continue;
        }

        let abs_path = args.repo_path.join(relative_path);
        let file_bytes = match std::fs::read(&abs_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %relative_path, error = %e, "lecture fichier échouée — skip");
                continue;
            }
        };

        let hash = content_hash_source(&file_bytes);

        // Idempotence: skip if hash unchanged.
        if let Some(stored_hash) = indexed_paths.get(relative_path.as_str()) {
            if *stored_hash == hash {
                report.files_skipped += 1;
                continue;
            }
        }

        // tree-sitter parse.
        let content = match std::str::from_utf8(&file_bytes) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %relative_path, error = %e, "fichier non-UTF8 — skip");
                continue;
            }
        };

        let symbols =
            match parse_rust_file(relative_path, content, args.visibility.include_private()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        path = %relative_path,
                        error = %e,
                        "parse tree-sitter échoué — skip (accuracy>coverage)"
                    );
                    continue;
                }
            };

        let notes = build_derived_notes(&args.vault_id, symbols);
        let note_count = notes.len();

        // Atomic write.
        index
            .write_note_derived_batch(&args.vault_id, relative_path, &hash, &head_sha, notes)
            .await
            .with_context(|| format!("write_note_derived_batch pour {relative_path}"))?;

        report.files_ingested += 1;
        report.notes_inserted += note_count;
    }

    // ── Phase 2: deletion propagation ────────────────────────────────────────

    let git_set: std::collections::HashSet<&str> = git_files.iter().map(|s| s.as_str()).collect();

    for indexed_path in indexed_paths.keys() {
        if !git_set.contains(indexed_path.as_str()) {
            // source_path absent from git ls-files → delete notes.
            // write_note_derived_batch with notes=[] deletes existing notes.
            // IMPORTANT: the 1st argument is vault_id (not indexed_path).
            index
                .write_note_derived_batch(&args.vault_id, indexed_path, "", "", vec![])
                .await
                .with_context(|| format!("delete_path_notes pour {indexed_path}"))?;

            // Nettoyer l'entrée code_freshness.
            index
                .delete_code_freshness_entry(&args.vault_id, indexed_path)
                .await
                .with_context(|| format!("delete_code_freshness_entry pour {indexed_path}"))?;

            report.files_deleted += 1;
        }
    }

    report.duration_ms = start.elapsed().as_millis() as u64;

    // Remove the incomplete-run marker — the run completed normally.
    // A removal error is non-fatal: the next run will detect the marker and
    // force a rebuild, which is the safe behaviour.
    if let Err(e) = std::fs::remove_file(&marker_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                marker = %marker_path.display(),
                error = %e,
                "impossible de retirer le marqueur ingest incomplet — le prochain run fera un rebuild"
            );
        }
    }

    tracing::info!(
        vault_id = %args.vault_id,
        files_total = report.files_total,
        files_ingested = report.files_ingested,
        files_skipped = report.files_skipped,
        files_deleted = report.files_deleted,
        notes_inserted = report.notes_inserted,
        duration_ms = report.duration_ms,
        "code ingest terminé"
    );

    Ok(report)
}

/// Arguments for the `code update` command.
#[derive(Debug, Clone, Default)]
pub struct CodeUpdateArgs {
    /// Path to the git repository to update.
    pub repo_path: PathBuf,
    /// Target logical vault (e.g. `"code-gradatum"`).
    pub vault_id: String,
    /// Path to the Gradatum index.
    pub index_path: PathBuf,
    /// Optional visibility mode override.
    ///
    /// `None` (default): the mode is read from the `code_vault.visibility` column
    /// (persisted at the last `code ingest`). Ensures consistency between ingest
    /// and update without requiring the operator to re-specify the flag each time.
    ///
    /// `Some(v)`: forces this mode for the current update and persists it in the
    /// database (identical behaviour to an ingest with this mode).
    pub visibility_override: Option<IngestVisibility>,
}

/// Report returned by an O(diff) update run.
#[derive(Debug, Default)]
pub struct UpdateReport {
    /// Number of `.rs` files changed (Added/Modified/Deleted) between the two shas.
    pub files_changed: usize,
    /// Number of files re-ingested (Added/Modified).
    pub files_ingested: usize,
    /// Number of files deleted (Deleted).
    pub files_deleted: usize,
    /// Total number of notes inserted.
    pub notes_inserted: usize,
    /// Starting sha (last known ingest) — empty on full-ingest fallback.
    pub from_sha: String,
    /// New HEAD sha.
    pub to_sha: String,
    /// Update duration in milliseconds.
    pub duration_ms: u64,
}

/// Updates a code vault in O(diff) from the last ingest.
///
/// ## Workflow
///
/// 1. Read the vault's last `ingested_sha` (`get_last_ingested_sha`).
///    - Absent → falls back to a full `run_ingest` (first ingest).
/// 2. `git diff --name-status <last_sha>..HEAD` → Added/Modified/Deleted `.rs` files.
/// 3. Re-ingest Added/Modified (atomic transaction per file).
/// 4. Delete Deleted files (deletion propagation).
/// 5. Store the new HEAD (via `ingested_sha` from batches + repo path).
///
/// Performance target: < 3 s after commit. Cost is proportional to the diff, not the repository.
///
/// ## Errors
///
/// Returns an error if the repository or index is invalid, or if `git diff` fails.
/// Unparseable files are silently skipped (accuracy over coverage).
pub async fn run_update(args: CodeUpdateArgs) -> Result<UpdateReport> {
    let start = std::time::Instant::now();

    if !args.index_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le worker doit avoir démarré au moins une fois",
            args.index_path.display()
        );
    }

    let index = SqliteIndex::open(&args.index_path)
        .await
        .with_context(|| format!("ouverture index.db : {}", args.index_path.display()))?;

    // ── Detect incomplete-run marker ──────────────────────────────────────────
    //
    // If the marker is present, the previous run was interrupted. A partial diff
    // from a `last_sha` potentially from an inconsistent state is unsafe →
    // fall back to a full ingest with rebuild.
    let safe_vault = args
        .vault_id
        .replace(|c: char| !c.is_alphanumeric() && c != '-', "_");
    let marker_path = args
        .index_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!(".ingest-incomplete-{safe_vault}"));

    if marker_path.exists() {
        tracing::warn!(
            vault_id = %args.vault_id,
            "run_update : marqueur ingest incomplet détecté — diff partiel refusé. Rebuild complet."
        );
        let to_sha = git_head_sha(&args.repo_path)?;
        let ingest = run_ingest(CodeIngestArgs {
            repo_path: args.repo_path,
            vault_id: args.vault_id,
            index_path: args.index_path,
            rebuild: true,
            visibility: args.visibility_override.unwrap_or_default(),
        })
        .await?;
        return Ok(UpdateReport {
            files_changed: ingest.files_ingested + ingest.files_deleted,
            files_ingested: ingest.files_ingested,
            files_deleted: ingest.files_deleted,
            notes_inserted: ingest.notes_inserted,
            from_sha: String::new(),
            to_sha,
            duration_ms: start.elapsed().as_millis() as u64,
        });
    }

    // Last ingested sha for this vault.
    let last_sha = index
        .get_last_ingested_sha(&args.vault_id)
        .await
        .with_context(|| format!("get_last_ingested_sha({})", args.vault_id))?;

    // Determine the effective visibility mode:
    // 1. Explicit CLI override → use directly.
    // 2. Absent → read from code_vault.visibility (persisted at last ingest).
    // 3. Unknown vault (no code_vault row) → fallback to Pub (accuracy over coverage).
    let effective_visibility = if let Some(ov) = args.visibility_override {
        ov
    } else {
        let stored = index
            .get_code_vault_visibility(&args.vault_id)
            .await
            .with_context(|| format!("get_code_vault_visibility({})", args.vault_id))?;
        match stored {
            Some(s) => IngestVisibility::from_db_str(&s),
            None => {
                tracing::debug!(
                    vault_id = %args.vault_id,
                    "code update : vault inconnu ou pré-0018 → visibilité Pub par défaut"
                );
                IngestVisibility::Pub
            }
        }
    };

    let Some(from_sha) = last_sha else {
        // No prior ingest → fall back to a full ingest (first pass).
        tracing::info!(
            vault_id = %args.vault_id,
            visibility = effective_visibility.as_str(),
            "code update : aucun sha précédent → fallback ingest complet"
        );
        let to_sha = git_head_sha(&args.repo_path)?;
        let ingest = run_ingest(CodeIngestArgs {
            repo_path: args.repo_path,
            vault_id: args.vault_id,
            index_path: args.index_path,
            rebuild: false,
            visibility: effective_visibility,
        })
        .await?;
        return Ok(UpdateReport {
            files_changed: ingest.files_ingested + ingest.files_deleted,
            files_ingested: ingest.files_ingested,
            files_deleted: ingest.files_deleted,
            notes_inserted: ingest.notes_inserted,
            from_sha: String::new(),
            to_sha,
            duration_ms: start.elapsed().as_millis() as u64,
        });
    };

    let head_sha = git_head_sha(&args.repo_path)?;

    // Update the repo path and effective visibility mode.
    // (path may have changed since the initial ingest; mode may be overridden via CLI).
    let repo_abs = std::fs::canonicalize(&args.repo_path)
        .with_context(|| format!("canonicalize repo_path : {}", args.repo_path.display()))?;
    index
        .set_code_vault_repo_path_with_visibility(
            &args.vault_id,
            &repo_abs.to_string_lossy(),
            effective_visibility.as_str(),
        )
        .await
        .with_context(|| {
            format!(
                "set_code_vault_repo_path_with_visibility({})",
                args.vault_id
            )
        })?;

    // git diff --name-status <from>..HEAD
    let changes = git_diff_name_status(&args.repo_path, &from_sha, &head_sha)?;

    let mut report = UpdateReport {
        from_sha: from_sha.clone(),
        to_sha: head_sha.clone(),
        ..Default::default()
    };

    for (status, path) in &changes {
        if !path.ends_with(".rs") {
            continue;
        }
        report.files_changed += 1;

        match status {
            // Deleted → delete notes for the source_path (atomic transaction).
            DiffStatus::Deleted => {
                index
                    .write_note_derived_batch(&args.vault_id, path, "", "", vec![])
                    .await
                    .with_context(|| format!("delete notes pour {path}"))?;
                index
                    .delete_code_freshness_entry(&args.vault_id, path)
                    .await
                    .with_context(|| format!("delete_code_freshness_entry pour {path}"))?;
                report.files_deleted += 1;
            }
            // Added/Modified → re-ingest the file (atomic transaction).
            DiffStatus::AddedOrModified => {
                // Defense-in-depth: same guard as in run_ingest.
                // `git diff --name-status -z --no-renames` never returns absolute paths,
                // but the guard closes the PathBuf::join(absolute) vector.
                if std::path::Path::new(path.as_str()).is_absolute() || path.starts_with('/') {
                    tracing::warn!(
                        path = %path,
                        "git diff a retourné un path absolu inattendu — skip défensif"
                    );
                    continue;
                }

                let abs_path = args.repo_path.join(path);
                let file_bytes = match std::fs::read(&abs_path) {
                    Ok(b) => b,
                    Err(e) => {
                        // File listed as Modified but unreadable (race condition) → defensive skip.
                        tracing::warn!(path = %path, error = %e, "lecture échouée — skip");
                        continue;
                    }
                };
                let content = match std::str::from_utf8(&file_bytes) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(path = %path, error = %e, "non-UTF8 — skip");
                        continue;
                    }
                };
                let hash = content_hash_source(&file_bytes);
                let symbols =
                    match parse_rust_file(path, content, effective_visibility.include_private()) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(path = %path, error = %e, "parse échoué — skip");
                            continue;
                        }
                    };
                let notes = build_derived_notes(&args.vault_id, symbols);
                let note_count = notes.len();
                index
                    .write_note_derived_batch(&args.vault_id, path, &hash, &head_sha, notes)
                    .await
                    .with_context(|| format!("write_note_derived_batch pour {path}"))?;
                report.files_ingested += 1;
                report.notes_inserted += note_count;
            }
        }
    }

    report.duration_ms = start.elapsed().as_millis() as u64;

    tracing::info!(
        vault_id = %args.vault_id,
        files_changed = report.files_changed,
        files_ingested = report.files_ingested,
        files_deleted = report.files_deleted,
        notes_inserted = report.notes_inserted,
        from_sha = %report.from_sha,
        to_sha = %report.to_sha,
        duration_ms = report.duration_ms,
        "code update terminé"
    );

    Ok(report)
}

/// Status of a file in a `git diff --name-status` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffStatus {
    /// Added (A) or Modified (M) — re-ingest.
    AddedOrModified,
    /// Deleted (D) — delete notes.
    Deleted,
}

/// `git diff --name-status <from>..<to>` → list of (status, path) pairs.
///
/// Renames (R) are decomposed into (Deleted old, Added new) via `--no-renames`:
/// R becomes D+A, which simplifies propagation (new path re-ingested, old path deleted).
/// Copies are treated identically.
///
/// ## Unicode and space-safe paths
///
/// `-z` (NUL-terminated output) combined with `-c core.quotepath=off` prevents git
/// from quoting non-ASCII or space-containing paths. Parsing splits on `\0` instead
/// of `\n`, which is correct regardless of path content.
/// Without `-z`, git quotes non-ASCII paths (`"caf\303\251.rs"`) and a path with
/// a space would be split into two phantom entries on newline splitting.
fn git_diff_name_status(repo: &Path, from: &str, to: &str) -> Result<Vec<(DiffStatus, String)>> {
    let range = format!("{from}..{to}");
    let output = Command::new("git")
        // `-c core.quotepath=off`: disables C-style quoting for non-ASCII paths.
        // `-z`: NUL terminator — each field (status, path) is followed by '\0'.
        //       With --name-status -z, the format is: STATUS\0PATH\0STATUS\0PATH\0...
        .args([
            "-c",
            "core.quotepath=off",
            "diff",
            "--name-status",
            "--no-renames",
            "-z",
            &range,
        ])
        .current_dir(repo)
        .output()
        .with_context(|| format!("git diff --name-status -z {range} dans {}", repo.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "git diff échoué dans {} (range {range}) : {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // With -z, each field is NUL-terminated. Format:
    //   STATUS\0PATH\0STATUS\0PATH\0[...]
    // Split on '\0', filter empty strings (trailing NUL), then read pairs.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut fields: std::collections::VecDeque<&str> =
        stdout.split('\0').filter(|s| !s.is_empty()).collect();

    let mut result = Vec::new();
    while fields.len() >= 2 {
        let status_code = fields.pop_front().unwrap_or("");
        let path = fields.pop_front().unwrap_or("").to_string();

        if path.is_empty() {
            continue;
        }
        // First character of the status code (handles M, MM, etc.).
        let status = match status_code.chars().next() {
            Some('A') | Some('M') | Some('T') => DiffStatus::AddedOrModified,
            Some('D') => DiffStatus::Deleted,
            // C (copied) without --no-renames should not appear; ignore others.
            _ => continue,
        };
        result.push((status, path));
    }
    Ok(result)
}

/// Returns the HEAD sha of the git repository.
fn git_head_sha(repo: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("git rev-parse HEAD dans {}", repo.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse HEAD échoué dans {} : {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Returns the list of versioned files (`git ls-files`).
///
/// ## Unicode and space-safe paths
///
/// `-z` (NUL-terminated output) combined with `-c core.quotepath=off` correctly
/// handles paths containing spaces, accents, or non-ASCII characters.
/// Without `-z`, git quotes non-ASCII paths (`"caf\303\251.rs"`) and a path with
/// a space would be treated as two separate entries on newline splitting.
fn git_ls_files(repo: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        // `-c core.quotepath=off`: disables C-style quoting for non-ASCII paths.
        // `-z`: each path is NUL-terminated instead of newline-terminated.
        .args(["-c", "core.quotepath=off", "ls-files", "-z"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("git ls-files -z dans {}", repo.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "git ls-files échoué dans {} : {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}
