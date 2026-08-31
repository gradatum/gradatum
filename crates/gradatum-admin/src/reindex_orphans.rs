//! `gradatum-admin reindex-orphans` sub-command (F-166).
//!
//! Re-indexes notes **present on disk but absent from the index** — the orphans left by
//! the 2026-05-08/09 bulk import, which wrote `.md` files straight to disk, outside the
//! write funnel. The drift is strictly one-way (disk has more than the index): this command
//! closes it.
//!
//! ## Why a dedicated admin command
//!
//! The `ReIndex` API entry point is a deliberate `400` stub (`jobs_v2.rs`) that several
//! cards rely on — it is **not** re-opened here. The durable answer, consistent with the
//! `backfill-*` / `repair-*` family, is this operator sub-command.
//!
//! ## Convergence with the write funnel (the non-negotiable property)
//!
//! Re-indexing MUST converge with `Vault::write_note_inner`, so an orphan gains the exact
//! same three artefacts a normal write produces:
//! 1. an **index entry** (`notes` row + FTS);
//! 2. a **drift footprint** in `file_checksums` (without which the note would recreate the
//!    very hole F-165/F-176 close — an orphan re-indexed without its checksum is a regression);
//! 3. an **embed job** enqueued into the live `gradatum_jobs` queue.
//!
//! Artefacts (1) and (2) are obtained by **reusing** the public funnel entry
//! [`gradatum_vault::Vault::write_note_with_id`] — never a second implementation of the write
//! path. Artefact (3) reuses the exact enqueue primitives of `backfill-embeddings`
//! (`build_embed_job` and friends), so the "count read back
//! from the table" property is inherited unchanged.
//!
//! ### Accepted effect of reusing the funnel
//!
//! The funnel sets `frontmatter.updated = Utc::now()` and rewrites the `.md` on every write.
//! Re-indexing an orphan therefore bumps its `updated` and re-serialises the file. This is
//! **the funnel's write** — the single source of the index/`file_checksums`/embed coherence,
//! not a side effect to be dodged. Avoiding it would mean duplicating the write path, which
//! is worse than the bump.
//!
//! ## Duplicate-`.md` guard
//!
//! `Vault::write_note_inner` computes the on-disk path from `(vault_id, locus, id)`; it does
//! **not** know the legacy section-as-locus layout. If an orphan physically lived at a
//! non-canonical path (e.g. `main/<section>/<id>.md` while its frontmatter carries no locus),
//! the funnel would write a *second* `.md` at the canonical path and leave the original
//! behind — a silent duplicate. This command refuses such notes (`Outcome::SkippedPathMismatch`)
//! rather than duplicate them: the mismatch is surfaced, never written. The live `main` vault
//! is entirely flat (`main/<id>.md`), so the guard is inert there; it keeps the command safe
//! for any tenant/layout.
//!
//! ## Idempotence and resumption
//!
//! Orphans are the set-difference `on-disk ULIDs − indexed ULIDs`, recomputed each run. A
//! re-indexed note leaves the set the moment its index row exists, so a second run finds zero
//! orphans. The scan is its own resumption point.
//!
//! ## Usage
//! ```text
//! gradatum-admin reindex-orphans --root /var/lib/gradatum --dry-run
//! gradatum-admin reindex-orphans --root /var/lib/gradatum
//! gradatum-admin reindex-orphans --root /var/lib/gradatum --tenant code-gradatum --limit 100
//! ```
//!
//! ## Expected paths (standard install layout)
//! - Queue : `<root>/db/queue.sqlite`
//! - Index : `<root>/vault/.gradatum/index.db`
//! - Notes : `<root>/vault/<tenant>/<id>.md`
//!
//! A backup before running against a production vault is strongly recommended.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gradatum_core::identity::NoteId;
use gradatum_core::paths::{queue_db_path, vault_index_path};
use gradatum_vault::Vault;
use ulid::Ulid;
use walkdir::WalkDir;

use crate::backfill_embeddings::{enqueue_and_verify, guard_tenant_scope, open_queue_pool};
use gradatum_db_sqlite::SqliteQueueStore;

/// Arguments for the `reindex-orphans` sub-command.
#[derive(Debug, Clone)]
pub struct ReindexOrphansArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`) — holds `vault/` and `db/`.
    pub root: PathBuf,
    /// Target tenant / `vault_id` (default: `"main"`).
    pub tenant: Option<String>,
    /// Maximum number of orphans to re-index; unlimited when absent.
    pub limit: Option<usize>,
    /// Preview actions without writing anything.
    pub dry_run: bool,
}

/// Report of a `reindex-orphans` run.
#[derive(Debug, Default, Clone)]
#[must_use = "the report states how many orphans were re-indexed and how many were skipped"]
pub struct ReindexOrphansReport {
    /// Orphans found: `.md` on disk whose ULID is absent from the index (after `--limit`).
    pub orphans_found: usize,
    /// Notes re-indexed. **Read back from the index** in a real run (never the loop counter);
    /// in dry-run this is the count that *would* be re-indexed (a pre-flight preview).
    pub reindexed: usize,
    /// Embed jobs confirmed present in `gradatum_jobs` (read back from the queue). Always `0`
    /// in dry-run.
    pub embed_enqueued: usize,
    /// Orphans skipped because their `.md` did not parse (surfaced, not silenced).
    pub skipped_malformed: usize,
    /// Orphans skipped because their physical path is not the funnel-canonical one — writing
    /// would create a duplicate `.md` (surfaced, never written).
    pub skipped_path_mismatch: usize,
    /// `true` when the run was a dry-run.
    pub dry_run: bool,
}

/// One orphan discovered on disk: its ULID and its path relative to the vault directory.
#[derive(Debug, Clone)]
struct Orphan {
    id: NoteId,
    /// Path relative to `<root>/vault`, e.g. `main/01ID.md`.
    relative_path: String,
}

/// Per-orphan outcome, folded into the report by the caller.
enum Outcome {
    Reindexed(NoteId),
    SkippedMalformed,
    SkippedPathMismatch,
}

/// Canonical on-disk path a note takes through the funnel, relative to the vault directory.
///
/// Mirror of the private `gradatum_vault::note_md_relative_path`. Kept in lockstep on
/// purpose: any divergence would let the duplicate-`.md` guard pass a note the funnel then
/// writes elsewhere. The parity is covered by [`tests::canonical_path_matches_funnel_shape`].
fn canonical_relative_path(vault_id: &str, locus: Option<&str>, id: &NoteId) -> String {
    match locus {
        Some(loc) => format!("{vault_id}/{loc}/{id}.md"),
        None => format!("{vault_id}/{id}.md"),
    }
}

/// Entry point. Scans for orphans, guards the volume, then (unless dry-run) re-indexes each
/// through the write funnel and enqueues its embed job.
///
/// # Errors
/// - `index.db` or `queue.sqlite` absent → descriptive error.
/// - The volume guard refuses an unbounded mass operation (see `guard_tenant_scope`).
/// - A filesystem or SQLite error during the scan, write or enqueue.
pub async fn run(args: ReindexOrphansArgs) -> Result<ReindexOrphansReport> {
    let index_path = vault_index_path(&args.root);
    let queue_path = queue_db_path(&args.root);
    if !index_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the server must have started at least once",
            index_path.display()
        );
    }
    if !queue_path.exists() {
        anyhow::bail!(
            "queue.sqlite not found: {} — run `gradatum-admin init` first",
            queue_path.display()
        );
    }

    let tenant = args.tenant.as_deref().unwrap_or("main").to_string();
    let vault_dir = args.root.join("vault");

    // ── Scan (synchrone) : orphelins = ULIDs sur disque absents de l'index ────────
    let orphans = scan_orphans(&index_path, &vault_dir, &tenant, args.limit)
        .context("scanning disk for orphan notes")?;

    if orphans.is_empty() {
        eprintln!(
            "reindex-orphans: 0 orphan — the index already covers all .md files on disk (tenant='{tenant}')"
        );
        return Ok(ReindexOrphansReport {
            dry_run: args.dry_run,
            ..Default::default()
        });
    }

    let orphans_found = orphans.len();

    // Garde-fou AVANT toute écriture : refuse une ré-indexation de masse non bornée.
    // MÊME garde que backfill-embeddings (réutilisée, pas recopiée).
    guard_tenant_scope(&tenant, orphans_found, args.limit)?;

    if args.dry_run {
        // Pré-vol : compter ceux qui PASSERAIENT les gardes (parse + chemin canonique),
        // sans ouvrir le vault ni écrire quoi que ce soit.
        let mut would = 0usize;
        let mut malformed = 0usize;
        let mut mismatch = 0usize;
        for orphan in &orphans {
            match classify_dry(&vault_dir, &tenant, orphan) {
                Outcome::Reindexed(_) => would += 1,
                Outcome::SkippedMalformed => malformed += 1,
                Outcome::SkippedPathMismatch => mismatch += 1,
            }
        }
        eprintln!(
            "reindex-orphans [DRY-RUN]: {orphans_found} orphan(s) — {would} re-indexable, \
             {malformed} .md unreadable, {mismatch} non-canonical path(s) (tenant='{tenant}')"
        );
        return Ok(ReindexOrphansReport {
            orphans_found,
            reindexed: would,
            embed_enqueued: 0,
            skipped_malformed: malformed,
            skipped_path_mismatch: mismatch,
            dry_run: true,
        });
    }

    eprintln!(
        "reindex-orphans: {orphans_found} orphan(s) (tenant='{tenant}') — re-indexing through the funnel..."
    );

    // Ouvre le vault CIBLE : sa méthode publique `write_note_with_id` EST l'entonnoir
    // (index + file_checksums + CoW). Aucune ré-implémentation du chemin d'écriture.
    let vault = Vault::open(&vault_dir)
        .await
        .map_err(|e| anyhow::anyhow!("ouverture du vault {}: {e}", vault_dir.display()))?;

    let mut report = ReindexOrphansReport {
        orphans_found,
        dry_run: false,
        ..Default::default()
    };
    let mut reindexed_ids: Vec<NoteId> = Vec::with_capacity(orphans_found);

    for orphan in &orphans {
        match reindex_one(&vault, &vault_dir, &tenant, orphan).await? {
            Outcome::Reindexed(id) => reindexed_ids.push(id),
            Outcome::SkippedMalformed => report.skipped_malformed += 1,
            Outcome::SkippedPathMismatch => report.skipped_path_mismatch += 1,
        }
    }

    // ── Compteur relu DANS l'index (jamais reindexed_ids.len()) ───────────────────
    // Une écriture funnel restée sans effet doit se voir : le compte vient de la table.
    report.reindexed = count_present_in_index(&index_path, &tenant, &reindexed_ids)
        .context("re-reading re-indexed notes from index.db")?;

    // ── Enfilage embed pour les notes ré-indexées (convergence artefact #3) ───────
    // Réutilise les primitives de backfill-embeddings → compteur relu dans gradatum_jobs.
    if !reindexed_ids.is_empty() {
        let ulids: Vec<Ulid> = reindexed_ids.iter().map(|id| id.0).collect();
        let pool = open_queue_pool(&queue_path)
            .await
            .context("ouverture du pool queue.sqlite (WAL)")?;
        let store = SqliteQueueStore::new(pool);
        report.embed_enqueued = enqueue_and_verify(&store, &ulids, &tenant)
            .await
            .context("enqueueing Embed jobs for re-indexed notes")?;
    }

    tracing::info!(
        orphans_found = report.orphans_found,
        reindexed = report.reindexed,
        embed_enqueued = report.embed_enqueued,
        skipped_malformed = report.skipped_malformed,
        skipped_path_mismatch = report.skipped_path_mismatch,
        "reindex-orphans complete"
    );

    Ok(report)
}

/// Re-indexes one orphan through the funnel, after the parse + canonical-path guards.
///
/// `.md` is parsed first; a malformed file is surfaced ([`Outcome::SkippedMalformed`]) and
/// the run continues. A non-canonical physical path is refused
/// ([`Outcome::SkippedPathMismatch`]) so the funnel cannot create a duplicate `.md`.
async fn reindex_one(
    vault: &Vault,
    vault_dir: &Path,
    tenant: &str,
    orphan: &Orphan,
) -> Result<Outcome> {
    let md_path = vault_dir.join(&orphan.relative_path);
    let raw = std::fs::read_to_string(&md_path)
        .with_context(|| format!("reading .md {}", md_path.display()))?;

    let parsed = match gradatum_markdown::parse(&raw) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                id = %orphan.id,
                path = %orphan.relative_path,
                err = %e,
                "reindex-orphans: .md does not parse — skipped (malformed)"
            );
            return Ok(Outcome::SkippedMalformed);
        }
    };

    // Garde anti-doublon : le funnel écrira à `canonical_relative_path` ; s'il diffère du
    // chemin physique réel, il créerait un second .md. On refuse plutôt que de dupliquer.
    let locus = parsed.frontmatter.locus.as_ref().map(|l| l.as_str());
    let canonical = canonical_relative_path(tenant, locus, &orphan.id);
    if canonical != orphan.relative_path {
        tracing::warn!(
            id = %orphan.id,
            actual = %orphan.relative_path,
            canonical = %canonical,
            "reindex-orphans: non-canonical physical path — skipped (writing would duplicate the .md)"
        );
        return Ok(Outcome::SkippedPathMismatch);
    }

    // Entonnoir : write_note_with_id → index + file_checksums + CoW (create : la note est
    // absente de l'index, donc read-before-write rend NoteNotFound, pas de snapshot).
    vault
        .write_note_with_id(parsed.frontmatter, parsed.body.markdown, orphan.id)
        .await
        .map_err(|e| anyhow::anyhow!("funnel write for note {}: {e}", orphan.id))?;

    Ok(Outcome::Reindexed(orphan.id))
}

/// Dry-run classification: parse + canonical-path check only, no vault, no write.
fn classify_dry(vault_dir: &Path, tenant: &str, orphan: &Orphan) -> Outcome {
    let md_path = vault_dir.join(&orphan.relative_path);
    let raw = match std::fs::read_to_string(&md_path) {
        Ok(r) => r,
        Err(_) => return Outcome::SkippedMalformed,
    };
    let parsed = match gradatum_markdown::parse(&raw) {
        Ok(p) => p,
        Err(_) => return Outcome::SkippedMalformed,
    };
    let locus = parsed.frontmatter.locus.as_ref().map(|l| l.as_str());
    if canonical_relative_path(tenant, locus, &orphan.id) != orphan.relative_path {
        return Outcome::SkippedPathMismatch;
    }
    Outcome::Reindexed(orphan.id)
}

/// Scans the vault directory for orphans: `.md` files whose ULID is absent from the index.
///
/// Hidden directories (`.history/`, `.archive/`, `.gradatum/`) are pruned, and symlinks are
/// never followed (`follow_links(false)` — path-traversal safety, ADN 5). Files whose stem is
/// not a valid ULID (e.g. `README.md`) are ignored — they are not notes. Orphans are sorted
/// by ULID for determinism, then `--limit` is applied.
fn scan_orphans(
    index_path: &Path,
    vault_dir: &Path,
    tenant: &str,
    limit: Option<usize>,
) -> Result<Vec<Orphan>> {
    let indexed: HashSet<String> = load_indexed_ids(index_path, tenant)?;

    let tenant_dir = vault_dir.join(tenant);
    if !tenant_dir.exists() {
        return Ok(Vec::new());
    }

    let mut orphans: Vec<Orphan> = Vec::new();
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
        // Un nom de fichier non-ULID n'est pas une note (jamais candidat).
        let Ok(ulid) = Ulid::from_string(stem) else {
            continue;
        };
        if indexed.contains(stem) {
            continue; // déjà indexé → pas un orphelin
        }
        let rel = path
            .strip_prefix(vault_dir)
            .map_err(|e| anyhow::anyhow!("relativising {}: {e}", path.display()))?
            .to_string_lossy()
            .to_string();
        orphans.push(Orphan {
            id: NoteId(ulid),
            relative_path: rel,
        });
    }

    orphans.sort_by_key(|o| o.id.0);
    if let Some(n) = limit {
        orphans.truncate(n);
    }
    Ok(orphans)
}

/// `true` when the walked entry is a hidden file or directory (name starts with `.`).
///
/// Pruning at the directory level (via `filter_entry`) keeps `.history/`, `.archive/` and
/// `.gradatum/` out of the walk entirely, mirroring the drift scanner's exclusions.
fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|n| n.starts_with('.'))
}

/// Loads the set of note ULIDs already present in the index for `tenant`, read-only.
///
/// Opens `index.db` with `SQLITE_OPEN_READ_ONLY` — the scan never writes. Returns the ULID
/// strings so membership can be tested against a `.md` file stem directly.
fn load_indexed_ids(index_path: &Path, tenant: &str) -> Result<HashSet<String>> {
    let conn = rusqlite::Connection::open_with_flags(
        index_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .context("opening index.db read-only")?;
    let mut stmt = conn
        .prepare("SELECT id FROM notes WHERE vault_id = ?1")
        .context("preparing indexed-ids query")?;
    let rows = stmt
        .query_map(rusqlite::params![tenant], |row| row.get::<_, String>(0))
        .context("executing indexed-ids query")?;
    let mut set = HashSet::new();
    for row in rows {
        set.insert(row.context("reading indexed id")?);
    }
    Ok(set)
}

/// Counts how many of `ids` are now present in the index for `tenant`, read straight from
/// the table. This is the re-indexed counter — never the loop's own tally.
///
/// A funnel write that silently produced no row is therefore visible: the count comes from
/// `notes`, not from the number of notes the loop *believed* it wrote.
fn count_present_in_index(index_path: &Path, tenant: &str, ids: &[NoteId]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let conn = rusqlite::Connection::open_with_flags(
        index_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .context("opening index.db read-only for count")?;
    let mut present = 0usize;
    let mut stmt = conn
        .prepare("SELECT 1 FROM notes WHERE vault_id = ?1 AND id = ?2")
        .context("preparing count query")?;
    for id in ids {
        let found = stmt
            .exists(rusqlite::params![tenant, id.to_string()])
            .context("executing count query")?;
        if found {
            present += 1;
        }
    }
    Ok(present)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::scope::VaultId;
    use gradatum_db_sqlite::{open_queue_db, run_migrations};

    // ── canonical_relative_path : parité avec le funnel ──────────────────────────
    // Domaine où SEULE la forme de chemin peut faire échouer.
    #[test]
    fn canonical_path_matches_funnel_shape() {
        let id = NoteId::new();
        // Sans locus → racine tenant (le cas du vault main, plat).
        assert_eq!(
            canonical_relative_path("main", None, &id),
            format!("main/{id}.md")
        );
        // Avec locus → sous-répertoire.
        assert_eq!(
            canonical_relative_path("main", Some("knowledge/rust"), &id),
            format!("main/knowledge/rust/{id}.md")
        );
    }

    // ── scan_orphans : set-difference disque − index, exclusions cachées ─────────
    // Aucun nombre gravé : on teste des appartenances, pas des cardinaux mesurés.
    fn write_note_md(vault_dir: &Path, rel: &str, status: &str) {
        let path = vault_dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = format!(
            "---\nschema_version: 1\nvault_id: main\nsection: decisions\nstatus: {status}\n\
             created: \"2026-05-08T10:00:00Z\"\ntags:\n  - import\n---\n\n# Titre\n\nCorps.\n"
        );
        std::fs::write(path, raw).unwrap();
    }

    fn empty_index(dir: &Path) -> PathBuf {
        // Index minimal : table `notes(id, vault_id)` suffit pour le scan.
        let idx = dir.join("index.db");
        let conn = rusqlite::Connection::open(&idx).unwrap();
        conn.execute_batch(
            "CREATE TABLE notes (id TEXT NOT NULL, vault_id TEXT NOT NULL, PRIMARY KEY (vault_id, id));",
        )
        .unwrap();
        idx
    }

    #[test]
    fn scan_flags_disk_notes_absent_from_index_and_excludes_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_dir = tmp.path().join("vault");
        std::fs::create_dir_all(vault_dir.join("main")).unwrap();

        let orphan = Ulid::generate();
        let indexed = Ulid::generate();
        let archived = Ulid::generate();

        write_note_md(&vault_dir, &format!("main/{orphan}.md"), "live");
        write_note_md(&vault_dir, &format!("main/{indexed}.md"), "live");
        // Note archivée sous `.archive/` : stem ULID ET absente de l'index — SEUL le pruning
        // des répertoires cachés empêche de la ré-indexer (sinon on ressusciterait un archivé).
        write_note_md(
            &vault_dir,
            &format!("main/.archive/main/{archived}.md"),
            "live",
        );
        // Fichier non-ULID : jamais une note.
        std::fs::write(vault_dir.join("main/README.md"), "# readme\n").unwrap();

        // L'index connaît `indexed`, pas `orphan`.
        let index_path = empty_index(tmp.path());
        {
            let conn = rusqlite::Connection::open(&index_path).unwrap();
            conn.execute(
                "INSERT INTO notes (id, vault_id) VALUES (?1, 'main')",
                rusqlite::params![indexed.to_string()],
            )
            .unwrap();
        }

        let got: HashSet<Ulid> = scan_orphans(&index_path, &vault_dir, "main", None)
            .unwrap()
            .into_iter()
            .map(|o| o.id.0)
            .collect();

        assert!(
            got.contains(&orphan),
            "la note absente de l'index est un orphelin"
        );
        assert!(
            !got.contains(&indexed),
            "une note déjà indexée n'est pas un orphelin"
        );
        assert!(
            !got.contains(&archived),
            "une note sous .archive/ ne doit jamais être scannée"
        );
    }

    // ── count_present_in_index : le compteur vient de la table, pas de l'entrée ──
    #[test]
    fn count_reads_the_table_not_the_input_length() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = empty_index(tmp.path());

        let present = NoteId::new();
        let absent = NoteId::new();
        {
            let conn = rusqlite::Connection::open(&index_path).unwrap();
            conn.execute(
                "INSERT INTO notes (id, vault_id) VALUES (?1, 'main')",
                rusqlite::params![present.to_string()],
            )
            .unwrap();
        }

        // On interroge DEUX ids, un seul est en base → 1, jamais 2 (la longueur de l'entrée).
        let n = count_present_in_index(&index_path, "main", &[present, absent]).unwrap();
        assert_eq!(
            n, 1,
            "le compteur doit refléter la table, pas le nombre d'ids passés"
        );
    }

    // ── Convergence bout-en-bout : orphelin → index + file_checksums + embed ─────
    async fn make_queue(root: &Path) {
        let db_dir = root.join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db = open_queue_db(&db_dir.join("queue.sqlite")).await.unwrap();
        run_migrations(&db).await.unwrap();
    }

    /// Writes an orphan `.md` on disk (bypassing the funnel) so the index has no row for it.
    fn drop_orphan_on_disk(vault_dir: &Path, id: &NoteId) {
        write_note_md(vault_dir, &format!("main/{id}.md"), "live");
    }

    #[tokio::test]
    async fn convergence_reindexes_into_index_checksums_and_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vault_dir = root.join("vault");

        // Vault initialisé (crée index.db + arborescence), puis fermé.
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .unwrap();
        make_queue(root).await;

        let orphan = NoteId::new();
        drop_orphan_on_disk(&vault_dir, &orphan);

        let report = run(ReindexOrphansArgs {
            root: root.to_path_buf(),
            tenant: None,
            limit: None,
            dry_run: false,
        })
        .await
        .unwrap();

        assert_eq!(report.orphans_found, 1);
        assert_eq!(
            report.reindexed, 1,
            "la note doit être présente dans l'index (relu en base)"
        );
        assert_eq!(
            report.embed_enqueued, 1,
            "un job Embed doit être confirmé dans la file"
        );

        let index_path = vault_index_path(root);
        // Artefact 1 : entrée d'index.
        let in_notes: bool = {
            let conn = rusqlite::Connection::open(&index_path).unwrap();
            conn.query_row(
                "SELECT 1 FROM notes WHERE vault_id='main' AND id=?1",
                rusqlite::params![orphan.to_string()],
                |_| Ok(true),
            )
            .unwrap_or(false)
        };
        assert!(in_notes, "artefact 1 : la note est dans `notes`");

        // Artefact 2 : empreinte de dérive dans file_checksums.
        let checksum_rows: i64 = {
            let conn = rusqlite::Connection::open(&index_path).unwrap();
            conn.query_row(
                "SELECT count(*) FROM file_checksums WHERE relative_path LIKE ?1",
                rusqlite::params![format!("%{orphan}.md")],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            checksum_rows, 1,
            "artefact 2 : une empreinte file_checksums pour la note"
        );

        // Artefact 3 : job Embed dans gradatum_jobs.
        let queue_path = queue_db_path(root);
        let embed_jobs: i64 = {
            let conn = rusqlite::Connection::open(&queue_path).unwrap();
            conn.query_row(
                "SELECT count(*) FROM gradatum_jobs WHERE kind='Embed'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(embed_jobs, 1, "artefact 3 : un job Embed enfilé");
    }

    #[tokio::test]
    async fn second_run_finds_no_orphan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vault_dir = root.join("vault");
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .unwrap();
        make_queue(root).await;

        let orphan = NoteId::new();
        drop_orphan_on_disk(&vault_dir, &orphan);

        let args = || ReindexOrphansArgs {
            root: root.to_path_buf(),
            tenant: None,
            limit: None,
            dry_run: false,
        };
        let first = run(args()).await.unwrap();
        assert_eq!(first.reindexed, 1);

        let second = run(args()).await.unwrap();
        assert_eq!(
            second.orphans_found, 0,
            "idempotence : plus aucun orphelin au 2e run"
        );
        assert_eq!(second.reindexed, 0);
    }

    // ── Garde anti-doublon : chemin physique non canonique → refus, jamais écrit ─
    // Domaine où SEULE la règle de chemin peut déclencher (parse OK, note absente de l'index).
    #[tokio::test]
    async fn non_canonical_path_is_skipped_never_duplicated() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vault_dir = root.join("vault");
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .unwrap();
        make_queue(root).await;

        // Orphelin au layout legacy `main/<section>/<id>.md`, frontmatter SANS locus →
        // le funnel écrirait à `main/<id>.md` = doublon. Doit être refusé.
        let orphan = NoteId::new();
        write_note_md(&vault_dir, &format!("main/decisions/{orphan}.md"), "live");

        let report = run(ReindexOrphansArgs {
            root: root.to_path_buf(),
            tenant: None,
            limit: None,
            dry_run: false,
        })
        .await
        .unwrap();

        assert_eq!(report.orphans_found, 1);
        assert_eq!(
            report.skipped_path_mismatch, 1,
            "chemin non canonique → skip"
        );
        assert_eq!(report.reindexed, 0, "aucune écriture");

        // Aucun .md dupliqué à la racine tenant.
        assert!(
            !vault_dir.join(format!("main/{orphan}.md")).exists(),
            "le funnel ne doit PAS avoir créé un doublon à la racine"
        );
    }

    // ── .md illisible → surface skipped_malformed, ne bloque pas les autres ──────
    #[tokio::test]
    async fn malformed_md_is_surfaced_and_others_proceed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vault_dir = root.join("vault");
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .unwrap();
        make_queue(root).await;

        let good = NoteId::new();
        let bad = NoteId::new();
        drop_orphan_on_disk(&vault_dir, &good);
        // .md sans frontmatter → parse échoue.
        std::fs::write(
            vault_dir.join(format!("main/{bad}.md")),
            "pas de frontmatter\n",
        )
        .unwrap();

        let report = run(ReindexOrphansArgs {
            root: root.to_path_buf(),
            tenant: None,
            limit: None,
            dry_run: false,
        })
        .await
        .unwrap();

        assert_eq!(report.orphans_found, 2);
        assert_eq!(report.skipped_malformed, 1, "le .md illisible est surfacé");
        assert_eq!(
            report.reindexed, 1,
            "l'autre orphelin est ré-indexé malgré tout"
        );
    }

    // ── dry-run : rien n'est écrit ──────────────────────────────────────────────
    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let vault_dir = root.join("vault");
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .unwrap();
        make_queue(root).await;

        let orphan = NoteId::new();
        drop_orphan_on_disk(&vault_dir, &orphan);

        let report = run(ReindexOrphansArgs {
            root: root.to_path_buf(),
            tenant: None,
            limit: None,
            dry_run: true,
        })
        .await
        .unwrap();

        assert!(report.dry_run);
        assert_eq!(report.orphans_found, 1);
        assert_eq!(
            report.reindexed, 1,
            "dry-run : compte ce qui SERAIT ré-indexé"
        );

        // Rien en base.
        let index_path = vault_index_path(root);
        let in_notes: i64 = {
            let conn = rusqlite::Connection::open(&index_path).unwrap();
            conn.query_row(
                "SELECT count(*) FROM notes WHERE vault_id='main'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(in_notes, 0, "dry-run n'écrit aucune ligne d'index");
    }
}
