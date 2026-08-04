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

use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use gradatum_index::SqliteIndex;
use gradatum_ingest::{
    DerivedSymbol, IngestError, build_derived_notes, content_hash_source, parse_bash_file,
    parse_python_file, parse_rust_file, parse_tsx_file, parse_typescript_file,
};

/// Source file extensions handled by the multi-language ingest pipeline.
///
/// Only the extensions listed here are processed by `run_ingest` and `run_update`; any
/// other file is skipped silently, favouring accuracy over coverage.
///
/// `.tsx` is routed to the dedicated TSX grammar rather than the plain TypeScript one, so
/// React components parse correctly. `parse_file_by_extension` holds the full routing
/// table.
pub const SUPPORTED_EXTENSIONS: &[&str] = &[".rs", ".py", ".sh", ".bash", ".ts", ".tsx"];

/// Number of consecutive parse failures, in any supported language, after which the
/// circuit breaker opens.
///
/// When that many files in a row fail to parse — a parser bug, corrupted files, or a
/// syntax the grammar does not accept — the ingest stops instead of silently skipping most
/// of the corpus.
///
/// Three is a deliberate compromise: it tolerates the occasional file that legitimately
/// fails to parse, such as one built around complex macros or generated at build time,
/// without letting a systemic failure go unnoticed.
pub const CODE_MAP_REBUILD_MAX_FAILURES: u32 = 3;

/// Maximum number of rebuild attempts allowed once an `.ingest-incomplete-<vault>` marker
/// has been detected.
///
/// ## Problem
///
/// If `run_ingest` fails systematically — a panic in the index, a corrupted database, an
/// out-of-memory kill — the marker is written again on every attempt. The next
/// `run_update` sees the marker, retries a rebuild, and fails again: an expensive infinite
/// loop.
///
/// ## Solution
///
/// The marker file holds the attempt counter as an ASCII decimal integer, for example
/// `"2"`.
///
/// - On detection, the counter is read. Once it reaches this limit the run gives up with
///   an error, and an operator has to remove the marker by hand.
/// - Before each rebuild, the counter is incremented and written back.
/// - After a successful run the marker is deleted, which resets the counter implicitly.
///
/// Three matches [`CODE_MAP_REBUILD_MAX_FAILURES`] and follows the same reasoning: it
/// absorbs a few transient failures, such as an accidental `SIGKILL` or a reboot, before
/// raising the alarm on a pathological loop.
pub const CODE_MAP_REBUILD_MAX_RETRY: u32 = 3;

/// Reads the rebuild attempt counter from the marker file.
///
/// The marker file holds an ASCII decimal integer, for example `"2"`. Returns `0` when the
/// file is missing, empty or unparsable.
///
/// Read and parse errors are deliberately swallowed and reported as `0` rather than
/// propagated: an unreadable counter must not block the ingest, and the worst case is one
/// extra attempt rather than a permanent deadlock.
pub fn read_marker_attempts(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// Writes the rebuild attempt counter into the marker file.
///
/// The content is an ASCII decimal integer, for example `"2"`.
///
/// # Errors
///
/// The filesystem write fails.
pub fn write_marker_attempts(path: &Path, count: u32) -> Result<()> {
    std::fs::write(path, count.to_string().as_bytes()).with_context(|| {
        format!(
            "write_marker_attempts: writing counter to {}",
            path.display()
        )
    })
}

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

/// Refuses to ingest into a `vault_id` that already belongs to the DATA registry
/// (`tenants`) — mirror guard of the one carried by `SqliteIndex::provision_vault` (lot REG).
///
/// ## Why this guard exists
///
/// The two registries carry incompatible lifecycles: `tenants` drives per-vault background
/// jobs (distill, decay, GC) while `code_vault` rows are regenerated on every refresh and
/// have no lifecycle at all. Ingesting derived notes into a vault that the crons iterate
/// would expose them to distill; provisioning a code vault would do the symmetric damage.
/// The invariant is `tenants` ∩ `code_vault` = ∅, and it needs a bar on **both** sides —
/// a `code-` prefix check alone does not cover an operator who ran
/// `admin vault create code-gradatum` beforehand.
///
/// ## Errors
///
/// Returns an error if the registry lookup fails (fail-closed: a lookup failure never
/// grants ingestion) or if `vault_id` is present in `tenants`.
async fn refuse_data_vault(index: &SqliteIndex, vault_id: &str) -> Result<()> {
    let status = index
        .get_tenant_status(vault_id)
        .await
        .with_context(|| format!("get_tenant_status({vault_id})"))?;
    if status.is_some() {
        anyhow::bail!(
            "vault '{vault_id}' belongs to the DATA registry (`tenants`) — code ingest \
             is refused: derived notes would become eligible for background jobs \
             (distill/decay). A vault belongs to exactly one registry."
        );
    }
    Ok(())
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
            "index.db not found: {} — the worker must have started at least once",
            args.index_path.display()
        );
    }

    let index = SqliteIndex::open(&args.index_path)
        .await
        .with_context(|| format!("ouverture index.db : {}", args.index_path.display()))?;

    // Lot REG — garde miroir AVANT toute écriture, marqueur d'atomicité compris.
    refuse_data_vault(&index, &args.vault_id).await?;

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
            // ── Garde-fou anti-boucle rebuild ────────────────────────────────────
            //
            // Si `run_ingest` crashe systématiquement (OOM, panic DB, corruption),
            // le marqueur est re-posé à chaque tentative → boucle infinie coûteuse.
            //
            // Mécanisme : le contenu du marqueur encode le nombre de tentatives
            // précédentes (ASCII décimal). Si >= CODE_MAP_REBUILD_MAX_RETRY,
            // on abandonne et on demande une intervention manuelle.
            let attempts = read_marker_attempts(&marker_path);
            if attempts >= CODE_MAP_REBUILD_MAX_RETRY {
                tracing::error!(
                    vault_id = %args.vault_id,
                    marker = %marker_path.display(),
                    attempts,
                    max = CODE_MAP_REBUILD_MAX_RETRY,
                    "anti-loop guard: {} failed rebuild attempts — \
                     aborting. Manual intervention required: remove the marker \
                     or repair the index.",
                    attempts
                );
                anyhow::bail!(
                    "too many failed rebuild attempts ({attempts} >= \
                     {CODE_MAP_REBUILD_MAX_RETRY}) for vault '{}' — \
                     marker: {}. Remove the marker manually to unblock.",
                    args.vault_id,
                    marker_path.display()
                );
            }

            // Incrémenter et réécrire le compteur AVANT le rebuild (le run peut crasher).
            write_marker_attempts(&marker_path, attempts + 1).with_context(|| {
                format!(
                    "write_marker_attempts before rebuild: {}",
                    marker_path.display()
                )
            })?;

            tracing::warn!(
                vault_id = %args.vault_id,
                marker = %marker_path.display(),
                attempt = attempts + 1,
                max = CODE_MAP_REBUILD_MAX_RETRY,
                "incomplete run marker detected — previous run interrupted. \
                 Rebuild attempt {}/{CODE_MAP_REBUILD_MAX_RETRY}.",
                attempts + 1
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
        // In rebuild mode this placement is redundant but idempotent :
        // `write_marker_attempts` a déjà mis à jour le contenu (compteur).
        // Pour le cas rebuild explicite (args.rebuild=true, marker absent) :
        // on pose le marqueur avec compteur 0 initial.
        if !marker_path.exists() {
            write_marker_attempts(&marker_path, 0).with_context(|| {
                format!("set marker before drop rebuild: {}", marker_path.display())
            })?;
        }

        let deleted = index
            .delete_vault_from_index(&args.vault_id)
            .await
            .with_context(|| format!("delete_vault_from_index({})", args.vault_id))?;
        tracing::info!(
            vault_id = %args.vault_id,
            deleted,
            rebuild_explicit = args.rebuild,
            rebuild_forced = marker_path.exists(),
            "rebuild: vault dropped"
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
    //   Le marqueur est posé avec compteur 0 si absent (premier run normal).
    //   Si présent (rebuild forcé depuis ce bloc), le compteur est conservé
    //   (write_marker_attempts l'a déjà mis à jour dans le bloc rebuild ci-dessus).
    //
    // Rebuild mode: le marqueur a déjà été écrit avec le compteur incrémenté.
    //   On ne réécrit PAS avec `b""` pour ne pas effacer le compteur.
    if !marker_path.exists() {
        write_marker_attempts(&marker_path, 0).with_context(|| {
            format!("pose marqueur ingest incomplet : {}", marker_path.display())
        })?;
    }

    // ── Phase 1: process git files ───────────────────────────────────────────

    // Circuit-breaker : compteur d'échecs CONSÉCUTIFS de parse_rust_file.
    // Remis à 0 à chaque succès de parse. Ouverture à N=CODE_MAP_REBUILD_MAX_FAILURES.
    // AtomicU32 local (CLI one-shot, single-threaded) — pas de static nécessaire.
    let consecutive_parse_failures = AtomicU32::new(0);

    'files: for relative_path in &git_files {
        // Filtre multi-langage : ne traiter que les extensions supportées.
        if !SUPPORTED_EXTENSIONS
            .iter()
            .any(|ext| relative_path.ends_with(ext))
        {
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
                "git ls-files returned an unexpected absolute path — defensive skip"
            );
            continue;
        }

        let abs_path = args.repo_path.join(relative_path);
        let file_bytes = match std::fs::read(&abs_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %relative_path, error = %e, "file read failed — skip");
                continue;
            }
        };

        let hash = content_hash_source(&file_bytes);

        // Idempotence: skip if hash unchanged.
        if let Some(stored_hash) = indexed_paths.get(relative_path.as_str())
            && *stored_hash == hash
        {
            report.files_skipped += 1;
            continue;
        }

        // tree-sitter parse.
        let content = match std::str::from_utf8(&file_bytes) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %relative_path, error = %e, "non-UTF8 file — skip");
                continue;
            }
        };

        // Dispatch multi-langage par extension.
        // `parse_file_by_extension` retourne None pour les extensions non supportées ;
        // mais comme le filtre SUPPORTED_EXTENSIONS ci-dessus l'a déjà exclu,
        // None ici signale une incohérence interne → on loggue et on skip.
        let parse_result = match parse_file_by_extension(
            relative_path,
            content,
            args.visibility.include_private(),
        ) {
            Some(r) => r,
            None => {
                tracing::warn!(
                    path = %relative_path,
                    "extension not routed despite the SUPPORTED_EXTENSIONS filter — defensive skip"
                );
                continue 'files;
            }
        };

        let symbols = match parse_result {
            Ok(s) => {
                // Succès → reset compteur circuit-breaker.
                consecutive_parse_failures.store(0, Ordering::Relaxed);
                s
            }
            Err(e) => {
                let failures = consecutive_parse_failures.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    path = %relative_path,
                    error = %e,
                    consecutive_failures = failures,
                    "tree-sitter parse failed — skip (accuracy>coverage)"
                );
                // Circuit-breaker : N échecs consécutifs = arrêt de l'ingest.
                if circuit_breaker_should_open(failures) {
                    tracing::error!(
                        vault_id = %args.vault_id,
                        n = CODE_MAP_REBUILD_MAX_FAILURES,
                        "code-map rebuild circuit-breaker open after {} consecutive failures \
                         — ingest interrupted. Check the parser or the source files.",
                        CODE_MAP_REBUILD_MAX_FAILURES
                    );
                    break 'files;
                }
                continue 'files;
            }
        };

        let notes = build_derived_notes(&args.vault_id, symbols);
        let note_count = notes.len();

        // Atomic write.
        index
            .write_note_derived_batch(&args.vault_id, relative_path, &hash, &head_sha, notes)
            .await
            .with_context(|| format!("write_note_derived_batch for {relative_path}"))?;

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
                .with_context(|| format!("delete_path_notes for {indexed_path}"))?;

            // Nettoyer l'entrée code_freshness.
            index
                .delete_code_freshness_entry(&args.vault_id, indexed_path)
                .await
                .with_context(|| format!("delete_code_freshness_entry for {indexed_path}"))?;

            report.files_deleted += 1;
        }
    }

    report.duration_ms = start.elapsed().as_millis() as u64;

    // Remove the incomplete-run marker — the run completed normally.
    // A removal error is non-fatal: the next run will detect the marker and
    // force a rebuild, which is the safe behaviour.
    if let Err(e) = std::fs::remove_file(&marker_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            marker = %marker_path.display(),
            error = %e,
            "cannot remove the incomplete ingest marker — the next run will rebuild"
        );
    }

    tracing::info!(
        vault_id = %args.vault_id,
        files_total = report.files_total,
        files_ingested = report.files_ingested,
        files_skipped = report.files_skipped,
        files_deleted = report.files_deleted,
        notes_inserted = report.notes_inserted,
        duration_ms = report.duration_ms,
        "code ingest complete"
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
            "index.db not found: {} — the worker must have started at least once",
            args.index_path.display()
        );
    }

    let index = SqliteIndex::open(&args.index_path)
        .await
        .with_context(|| format!("ouverture index.db : {}", args.index_path.display()))?;

    // Lot REG — garde miroir AVANT toute écriture (le repli `run_ingest` la rejoue,
    // c'est sans effet : la garde est idempotente et sans I/O d'écriture).
    refuse_data_vault(&index, &args.vault_id).await?;

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
            "run_update: incomplete ingest marker detected — partial diff refused. Full rebuild."
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
                    "code update: unknown or pre-0018 vault → Pub visibility by default"
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
            "code update: no previous sha → full ingest fallback"
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
        // Filtre multi-langage : même liste que run_ingest.
        if !SUPPORTED_EXTENSIONS.iter().any(|ext| path.ends_with(*ext)) {
            continue;
        }
        report.files_changed += 1;

        match status {
            // Deleted → delete notes for the source_path (atomic transaction).
            DiffStatus::Deleted => {
                index
                    .write_note_derived_batch(&args.vault_id, path, "", "", vec![])
                    .await
                    .with_context(|| format!("delete notes for {path}"))?;
                index
                    .delete_code_freshness_entry(&args.vault_id, path)
                    .await
                    .with_context(|| format!("delete_code_freshness_entry for {path}"))?;
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
                        "git diff returned an unexpected absolute path — defensive skip"
                    );
                    continue;
                }

                let abs_path = args.repo_path.join(path);
                let file_bytes = match std::fs::read(&abs_path) {
                    Ok(b) => b,
                    Err(e) => {
                        // File listed as Modified but unreadable (race condition) → defensive skip.
                        tracing::warn!(path = %path, error = %e, "read failed — skip");
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
                // Dispatch multi-langage par extension.
                let symbols = match parse_file_by_extension(
                    path,
                    content,
                    effective_visibility.include_private(),
                ) {
                    Some(Ok(s)) => s,
                    Some(Err(e)) => {
                        tracing::warn!(path = %path, error = %e, "parse failed — skip");
                        continue;
                    }
                    None => {
                        // Incohérence interne : le filtre SUPPORTED_EXTENSIONS l'aurait dû l'exclure.
                        tracing::warn!(path = %path, "extension not routed — defensive skip");
                        continue;
                    }
                };
                let notes = build_derived_notes(&args.vault_id, symbols);
                let note_count = notes.len();
                index
                    .write_note_derived_batch(&args.vault_id, path, &hash, &head_sha, notes)
                    .await
                    .with_context(|| format!("write_note_derived_batch for {path}"))?;
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
        "code update complete"
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
        .with_context(|| format!("git diff --name-status -z {range} in {}", repo.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "git diff failed in {} (range {range}): {}",
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
        .with_context(|| format!("git rev-parse HEAD in {}", repo.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse HEAD failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Reports whether the circuit breaker should open after `failures` consecutive failures.
///
/// Extracted as its own function so the threshold logic can be unit-tested without
/// involving tree-sitter or any I/O.
///
/// # Returns
///
/// `true` si `failures >= CODE_MAP_REBUILD_MAX_FAILURES` (circuit-breaker ouvert).
#[inline]
pub(crate) fn circuit_breaker_should_open(failures: u32) -> bool {
    failures >= CODE_MAP_REBUILD_MAX_FAILURES
}

/// Dispatches symbol extraction according to the source file extension.
///
/// Returns `None` when the extension is not supported, in which case the caller skips the
/// file silently, favouring accuracy over coverage.
///
/// ## Routing table
///
/// | Extension        | Parser                           |
/// |------------------|----------------------------------|
/// | `.rs`            | `parse_rust_file`                |
/// | `.py`            | `parse_python_file`              |
/// | `.sh`, `.bash`   | `parse_bash_file`                |
/// | `.ts`            | `parse_typescript_file`          |
/// | `.tsx`           | `parse_tsx_file` (JSX grammar)   |
///
/// ## Note on `.tsx`
///
/// `.tsx` files go through the dedicated TSX grammar, so React components and JSX parse
/// correctly. TypeScript declarations — functions, classes, interfaces, arrow functions —
/// are extracted; pure JSX fragments yield no symbols, which is the intended trade-off of
/// favouring accuracy over coverage.
///
/// ## The `include_private` parameter
///
/// Bash has no visibility modifier, so the parameter is ignored for `.sh` and `.bash`.
///
/// ## Side effects
///
/// None: the function is pure and delegates to pure parsers.
pub(crate) fn parse_file_by_extension(
    source_path: &str,
    content: &str,
    include_private: bool,
) -> Option<Result<Vec<DerivedSymbol>, IngestError>> {
    if source_path.ends_with(".rs") {
        Some(parse_rust_file(source_path, content, include_private))
    } else if source_path.ends_with(".py") {
        Some(parse_python_file(source_path, content, include_private))
    } else if source_path.ends_with(".sh") || source_path.ends_with(".bash") {
        // Bash n'a pas de concept de visibilité — include_private ignoré.
        Some(parse_bash_file(source_path, content))
    } else if source_path.ends_with(".ts") {
        Some(parse_typescript_file(source_path, content, include_private))
    } else if source_path.ends_with(".tsx") {
        // Utilise LANGUAGE_TSX (grammaire JSX) pour les fichiers React.
        Some(parse_tsx_file(source_path, content, include_private))
    } else {
        None
    }
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
        .with_context(|| format!("git ls-files -z in {}", repo.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "git ls-files failed in {}: {}",
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

// ── Tests unitaires circuit-breaker + dispatch multi-langage ─────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Vérifie que la constante vaut 3 (valeur contractuelle spec B).
    ///
    /// Un bump accidentel serait un breaking change comportemental — ce test
    /// en fait une régression détectable immédiatement.
    #[test]
    fn circuit_breaker_constant_is_three() {
        assert_eq!(
            CODE_MAP_REBUILD_MAX_FAILURES, 3,
            "CODE_MAP_REBUILD_MAX_FAILURES doit valoir 3 (spec B — modifier ce test si la valeur \
             est intentionnellement changée)"
        );
    }

    /// Le circuit-breaker NE s'ouvre PAS avant N échecs.
    ///
    /// Critère : `circuit_breaker_should_open(k) == false` pour tout k < N.
    #[test]
    fn circuit_breaker_does_not_open_before_n() {
        for k in 0..CODE_MAP_REBUILD_MAX_FAILURES {
            assert!(
                !circuit_breaker_should_open(k),
                "circuit-breaker ne doit PAS s'ouvrir à k={k} < CODE_MAP_REBUILD_MAX_FAILURES={CODE_MAP_REBUILD_MAX_FAILURES}"
            );
        }
    }

    /// Le circuit-breaker s'ouvre exactement à N échecs consécutifs.
    ///
    /// Simule `AtomicU32::fetch_add` pour N cycles consécutifs et vérifie
    /// que `circuit_breaker_should_open` retourne `true` exactement au Nième.
    #[test]
    fn circuit_breaker_opens_after_n_consecutive_failures() {
        let counter = AtomicU32::new(0);

        for i in 0..CODE_MAP_REBUILD_MAX_FAILURES {
            let failures = counter.fetch_add(1, Ordering::Relaxed) + 1;
            let should_open = circuit_breaker_should_open(failures);

            if i + 1 < CODE_MAP_REBUILD_MAX_FAILURES {
                assert!(
                    !should_open,
                    "circuit-breaker ne doit pas s'ouvrir à l'échec {}/{CODE_MAP_REBUILD_MAX_FAILURES}",
                    i + 1
                );
            } else {
                // Dernier échec — ouverture attendue.
                assert!(
                    should_open,
                    "circuit-breaker doit s'ouvrir exactement au {}e échec consécutif",
                    CODE_MAP_REBUILD_MAX_FAILURES
                );
            }
        }
    }

    /// Un succès remet le compteur à zéro — le circuit-breaker ne s'ouvre pas
    /// si les échecs sont entrecoupés de succès.
    ///
    /// Scénario : N-1 échecs → succès (reset) → N-1 échecs → pas d'ouverture.
    #[test]
    fn circuit_breaker_resets_on_success() {
        let counter = AtomicU32::new(0);
        let n = CODE_MAP_REBUILD_MAX_FAILURES;

        // Phase 1 : N-1 échecs consécutifs.
        for _ in 0..n - 1 {
            let failures = counter.fetch_add(1, Ordering::Relaxed) + 1;
            assert!(
                !circuit_breaker_should_open(failures),
                "pas encore ouvert après {} < {n} échecs",
                failures
            );
        }

        // Succès : reset à 0.
        counter.store(0, Ordering::Relaxed);
        assert_eq!(counter.load(Ordering::Relaxed), 0, "reset après succès");

        // Phase 2 : N-1 nouveaux échecs → toujours pas d'ouverture (compteur reparti de 0).
        for k in 0..n - 1 {
            let failures = counter.fetch_add(1, Ordering::Relaxed) + 1;
            assert!(
                !circuit_breaker_should_open(failures),
                "après reset, N-1 nouveaux échecs (k={k}) ne doivent pas ouvrir le circuit-breaker"
            );
        }
    }

    /// `circuit_breaker_should_open` est monotone : une fois ouvert,
    /// tout compteur supérieur reste ouvert.
    #[test]
    fn circuit_breaker_stays_open_beyond_n() {
        for k in CODE_MAP_REBUILD_MAX_FAILURES..CODE_MAP_REBUILD_MAX_FAILURES + 10 {
            assert!(
                circuit_breaker_should_open(k),
                "circuit-breaker doit rester ouvert pour k={k} >= CODE_MAP_REBUILD_MAX_FAILURES"
            );
        }
    }

    // ── Tests dispatch multi-langage ─────────────────────────────────────────

    /// Une extension inconnue retourne `None` (skip silencieux).
    #[test]
    fn dispatch_unknown_extension_returns_none() {
        assert!(
            parse_file_by_extension("foo.unknown", "content", false).is_none(),
            ".unknown doit retourner None"
        );
        assert!(
            parse_file_by_extension("README.md", "# readme", false).is_none(),
            ".md doit retourner None"
        );
        assert!(
            parse_file_by_extension("Cargo.toml", "[package]", false).is_none(),
            ".toml doit retourner None"
        );
    }

    /// `.rs` est routé vers le parser Rust et retourne des symboles.
    #[test]
    fn dispatch_rust_returns_symbols() {
        let src = "pub fn hello() -> u32 { 42 }";
        let result = parse_file_by_extension("src/lib.rs", src, false);
        assert!(result.is_some(), ".rs doit retourner Some(...)");
        let symbols = result
            .unwrap()
            .expect("parse Rust ne doit pas échouer sur src valide");
        assert!(
            !symbols.is_empty(),
            ".rs valide doit produire au moins un symbole ; trouvé 0"
        );
        assert!(
            symbols.iter().any(|s| s.qualified_name == "hello"),
            "la fn 'hello' doit être extraite ; symboles trouvés : {:?}",
            symbols
                .iter()
                .map(|s| &s.qualified_name)
                .collect::<Vec<_>>()
        );
        assert!(
            symbols.iter().all(|s| s.source_path == "src/lib.rs"),
            "source_path doit correspondre au chemin transmis"
        );
    }

    /// `.py` est routé vers le parser Python et retourne des symboles.
    #[test]
    fn dispatch_python_returns_symbols() {
        let src = "def greet(name):\n    return 'hello ' + name\n";
        let result = parse_file_by_extension("app/utils.py", src, false);
        assert!(result.is_some(), ".py doit retourner Some(...)");
        let symbols = result
            .unwrap()
            .expect("parse Python ne doit pas échouer sur src valide");
        assert!(
            !symbols.is_empty(),
            ".py valide doit produire au moins un symbole ; trouvé 0"
        );
        assert!(
            symbols.iter().any(|s| s.qualified_name == "greet"),
            "la fn 'greet' doit être extraite ; symboles : {:?}",
            symbols
                .iter()
                .map(|s| &s.qualified_name)
                .collect::<Vec<_>>()
        );
        // Vérifier que le kind correspond à "fn" (convention Python parser)
        assert!(
            symbols.iter().any(|s| s.kind == "fn"),
            "kind doit être 'fn' pour une fonction Python top-level"
        );
    }

    /// `.sh` est routé vers le parser Bash et retourne des symboles.
    #[test]
    fn dispatch_bash_sh_returns_symbols() {
        let src = "#!/bin/bash\nfunction do_thing() {\n  echo hello\n}\n";
        let result = parse_file_by_extension("scripts/run.sh", src, false);
        assert!(result.is_some(), ".sh doit retourner Some(...)");
        let symbols = result
            .unwrap()
            .expect("parse Bash ne doit pas échouer sur src valide");
        assert!(
            !symbols.is_empty(),
            ".sh valide doit produire au moins un symbole ; trouvé 0"
        );
        assert!(
            symbols.iter().any(|s| s.qualified_name == "do_thing"),
            "la fn 'do_thing' doit être extraite ; symboles : {:?}",
            symbols
                .iter()
                .map(|s| &s.qualified_name)
                .collect::<Vec<_>>()
        );
    }

    /// `.bash` est routé vers le même parser Bash que `.sh`.
    #[test]
    fn dispatch_bash_bash_returns_symbols() {
        let src = "function setup_env() {\n  export FOO=bar\n}\n";
        let result = parse_file_by_extension("scripts/setup.bash", src, false);
        assert!(result.is_some(), ".bash doit retourner Some(...)");
        let symbols = result
            .unwrap()
            .expect("parse Bash ne doit pas échouer sur src valide");
        assert!(
            !symbols.is_empty(),
            ".bash valide doit produire au moins un symbole ; trouvé 0"
        );
    }

    /// `.ts` est routé vers le parser TypeScript et retourne des symboles.
    #[test]
    fn dispatch_typescript_returns_symbols() {
        let src = "export function greet(name: string): string { return 'hi ' + name; }\n";
        let result = parse_file_by_extension("src/utils.ts", src, false);
        assert!(result.is_some(), ".ts doit retourner Some(...)");
        let symbols = result
            .unwrap()
            .expect("parse TS ne doit pas échouer sur src valide");
        assert!(
            !symbols.is_empty(),
            ".ts valide doit produire au moins un symbole ; trouvé 0"
        );
        assert!(
            symbols.iter().any(|s| s.qualified_name == "greet"),
            "la fn 'greet' doit être extraite ; symboles : {:?}",
            symbols
                .iter()
                .map(|s| &s.qualified_name)
                .collect::<Vec<_>>()
        );
    }

    /// `.tsx` est routé vers `parse_tsx_file` (LANGUAGE_TSX) et comprend les composants React.
    ///
    /// Vérifie que :
    /// 1. Le dispatch retourne Some(...) pour `.tsx`.
    /// 2. Le JSX ne fait pas échouer le parse (has_error absent).
    /// 3. La déclaration function extraite correctement.
    #[test]
    fn dispatch_tsx_routes_to_tsx_parser() {
        // Composant React avec JSX : LANGUAGE_TSX le parse correctement.
        let src = "export function Label({ n }: { n: number }) { return <span>{n}</span>; }\n";
        let result = parse_file_by_extension("components/Label.tsx", src, false);
        assert!(result.is_some(), ".tsx doit retourner Some(...)");
        let symbols = result
            .unwrap()
            .expect("parse .tsx avec JSX ne doit pas échouer avec LANGUAGE_TSX");
        assert!(
            !symbols.is_empty(),
            ".tsx avec composant React doit produire au moins un symbole ; trouvé 0"
        );
        assert_eq!(
            symbols[0].qualified_name, "Label",
            "le composant Label doit être extrait"
        );
    }

    /// `include_private=true` vs `include_private=false` sur `.rs` :
    /// le mode `All` doit inclure les items privés absents du mode `Pub`.
    #[test]
    fn dispatch_rust_include_private_flag_honored() {
        let src = "pub fn public_fn() {}\nfn private_fn() {}\n";
        let pub_symbols = parse_file_by_extension("src/lib.rs", src, false)
            .expect("Some")
            .expect("parse ok");
        let all_symbols = parse_file_by_extension("src/lib.rs", src, true)
            .expect("Some")
            .expect("parse ok");

        let pub_names: Vec<&str> = pub_symbols
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect();
        let all_names: Vec<&str> = all_symbols
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect();

        assert!(
            pub_names.contains(&"public_fn"),
            "public_fn doit être dans le mode Pub"
        );
        assert!(
            !pub_names.contains(&"private_fn"),
            "private_fn ne doit PAS être dans le mode Pub"
        );
        assert!(
            all_names.contains(&"public_fn"),
            "public_fn doit être dans le mode All"
        );
        assert!(
            all_names.contains(&"private_fn"),
            "private_fn doit être dans le mode All"
        );
    }

    /// `SUPPORTED_EXTENSIONS` contient les 6 extensions attendues.
    ///
    /// Ce test documente le contrat contractuel des extensions supportées.
    /// Un changement accidentel serait détecté immédiatement.
    #[test]
    fn supported_extensions_contains_all_six() {
        let expected = [".rs", ".py", ".sh", ".bash", ".ts", ".tsx"];
        for ext in &expected {
            assert!(
                SUPPORTED_EXTENSIONS.contains(ext),
                "SUPPORTED_EXTENSIONS doit contenir '{ext}'"
            );
        }
        assert_eq!(
            SUPPORTED_EXTENSIONS.len(),
            6,
            "SUPPORTED_EXTENSIONS doit avoir exactement 6 entrées"
        );
    }

    // ── Tests garde-fou anti-boucle rebuild ──────────────────────────────────

    /// La constante `CODE_MAP_REBUILD_MAX_RETRY` vaut 3 (valeur contractuelle).
    ///
    /// Cohérente avec `CODE_MAP_REBUILD_MAX_FAILURES`. Modifier ce test si
    /// la valeur est intentionnellement changée.
    #[test]
    fn rebuild_max_retry_constant_is_three() {
        assert_eq!(
            CODE_MAP_REBUILD_MAX_RETRY, 3,
            "CODE_MAP_REBUILD_MAX_RETRY doit valoir 3"
        );
    }

    /// `read_marker_attempts` retourne 0 sur un fichier absent.
    #[test]
    fn read_marker_attempts_returns_zero_on_missing_file() {
        let tmp = tempfile::NamedTempFile::new().expect("NamedTempFile");
        // Supprimer le fichier pour simuler l'absence.
        let path = tmp.path().to_path_buf();
        drop(tmp);
        assert!(!path.exists(), "précondition : fichier absent");
        assert_eq!(
            read_marker_attempts(&path),
            0,
            "fichier absent → compteur 0"
        );
    }

    /// `read_marker_attempts` retourne 0 sur un fichier vide (marqueur legacy `b""`).
    #[test]
    fn read_marker_attempts_returns_zero_on_empty_file() {
        let tmp = tempfile::NamedTempFile::new().expect("NamedTempFile");
        // Fichier vide = marqueur posé par l'ancienne implémentation.
        std::fs::write(tmp.path(), b"").expect("écriture fichier vide");
        assert_eq!(
            read_marker_attempts(tmp.path()),
            0,
            "fichier vide → compteur 0 (marqueur legacy)"
        );
    }

    /// `write_marker_attempts` + `read_marker_attempts` : round-trip.
    ///
    /// Écrire N puis relire doit retourner N.
    #[test]
    fn write_and_read_marker_attempts_round_trip() {
        let tmp = tempfile::NamedTempFile::new().expect("NamedTempFile");
        for n in [0u32, 1, 2, CODE_MAP_REBUILD_MAX_RETRY, 10] {
            write_marker_attempts(tmp.path(), n)
                .expect("write_marker_attempts ne doit pas échouer");
            assert_eq!(
                read_marker_attempts(tmp.path()),
                n,
                "round-trip : écriture {n} → lecture {n}"
            );
        }
    }

    /// `read_marker_attempts` + `write_marker_attempts` : le compteur au seuil
    /// est lu correctement, et les valeurs autour du seuil sont distinctes.
    ///
    /// Vérifie la frontière `< MAX_RETRY` (autorisé) vs `>= MAX_RETRY` (bloqué)
    /// en utilisant le round-trip I/O plutôt que des assertions sur constantes.
    #[test]
    fn marker_attempts_threshold_boundary_via_io() {
        let tmp = tempfile::NamedTempFile::new().expect("NamedTempFile");

        // Valeur juste sous le seuil → doit être < MAX_RETRY.
        let below = CODE_MAP_REBUILD_MAX_RETRY - 1;
        write_marker_attempts(tmp.path(), below).expect("write below");
        let read_below = read_marker_attempts(tmp.path());
        assert_eq!(read_below, below, "round-trip below seuil");
        assert!(
            read_below < CODE_MAP_REBUILD_MAX_RETRY,
            "below seuil : {read_below} doit être < {CODE_MAP_REBUILD_MAX_RETRY}"
        );

        // Valeur exactement au seuil → doit déclencher le guard (>= MAX_RETRY).
        write_marker_attempts(tmp.path(), CODE_MAP_REBUILD_MAX_RETRY).expect("write at seuil");
        let read_at = read_marker_attempts(tmp.path());
        assert_eq!(read_at, CODE_MAP_REBUILD_MAX_RETRY, "round-trip at seuil");
        assert!(
            read_at >= CODE_MAP_REBUILD_MAX_RETRY,
            "at seuil : {read_at} doit déclencher le guard (>= {CODE_MAP_REBUILD_MAX_RETRY})"
        );

        // Valeur au-dessus du seuil → doit aussi déclencher le guard.
        let above = CODE_MAP_REBUILD_MAX_RETRY + 1;
        write_marker_attempts(tmp.path(), above).expect("write above");
        let read_above = read_marker_attempts(tmp.path());
        assert_eq!(read_above, above, "round-trip above seuil");
        assert!(
            read_above >= CODE_MAP_REBUILD_MAX_RETRY,
            "above seuil : {read_above} doit déclencher le guard (>= {CODE_MAP_REBUILD_MAX_RETRY})"
        );
    }
}
