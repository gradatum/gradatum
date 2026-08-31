//! Helper drift scan — three-level pre-check before reconstruction.
//!
//! ## Algorithm
//!
//! For each entry in `file_checksums`:
//!
//! 1. **Level 1 — strict size**: if `on_disk_size == expected_size`
//!    AND `prefix-4 KB hash == expected_hash_prefix_4kb` → file is likely unchanged.
//!    Short-circuits ~99% of stable files (fast sequential reads).
//!
//! 2. **Level 3 — full SHA-256**: all other cases → full hash.
//!    Determines whether the file is actually modified (`mismatch`) or not (`match`).
//!
//! ## Caller responsibilities
//!
//! This helper returns a `DriftScanResult` with counters and the list of missing files.
//! Reconstruction (re-parse + re-index + re-embed) is the responsibility of the caller
//! (`gradatum-vault::drift_orchestrator`).
//!
//! Detection of "untracked" files (present on disk, absent from `file_checksums`)
//! is also the caller's responsibility — this module only checks existing entries.
//!
//! ## OpenDAL data path
//!
//! `scan_phase_a` accepts a `&dyn gradatum_storage::Storage` instead of a `&Path vault_root`.
//! The `relative_path` values from `file_checksums` entries are already relative —
//! directly compatible with the Storage contract (relative paths).
//! `stat` provides the on-disk size; `read` provides bytes for hashing.

use std::collections::HashSet;
use std::path::PathBuf;

use sha2::Digest as _;
use ulid::Ulid;

use gradatum_core::error::GradatumError;
use gradatum_core::index::FileKind;
// list_file_checksums is a pub(crate) inherent method on SqliteIndex — no trait needed.
use gradatum_storage::{Storage, StorageError};

use crate::SqliteIndex;

/// Result of a drift scan.
///
/// Holds file counters at each verification level and the list of files
/// missing on disk.
// `#[non_exhaustive]` (F-245) : la structure n'est plus constructible par littéral
// chez un consommateur externe — tout ajout de champ futur est additif.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct DriftScanResult {
    /// Files whose size + prefix-4 KB hash match (likely unchanged).
    /// Do not require a full hash.
    pub level2_prefix_match: u64,

    /// Files whose full hash matches after size or prefix divergence.
    /// May indicate a cosmetic change (mtime only, padding, etc.).
    pub level3_full_hash_match: u64,

    /// Files whose full hash differs — drift confirmed, reconstruction required.
    pub level3_full_hash_mismatch: u64,

    /// Paths of files absent on disk (note referenced but file deleted).
    ///
    /// Direction *index → disk*: a `file_checksums` entry with no file.
    pub missing: Vec<PathBuf>,

    /// Note `.md` files present on disk but **absent** from `file_checksums`
    /// (direction *disque → index*).
    ///
    /// `scan_phase_a` enumerates `file_checksums`: it is by construction blind to a
    /// file the index does not know about. This list closes that blind spot — it is the
    /// orphan population: any `.md` written outside the write funnel, which reindexing
    /// recovers.
    /// Hidden files (`.history/`, `.archive/`, `.gradatum/`) and files whose name is not
    /// a ULID (e.g. `README.md`) are excluded: those are not notes.
    pub untracked: Vec<PathBuf>,

    /// Number of **embeddable** notes with no embedding row in `note_embeddings`
    /// (dimension *vecteur*).
    ///
    /// An embeddable note (statuses `live`/`pending-review`/`staging`, derived from the SSOT
    /// [`NoteStatus::embeddable_default_sql_list`](gradatum_core::status::NoteStatus::embeddable_default_sql_list))
    /// with no vector is drift just like a divergent hash:
    /// semantic search cannot retrieve it. The filter is identical to the one the embedding
    /// backfill repairs — otherwise a repairable note would stay unreported.
    /// Counted at the index level (`LEFT JOIN note_embeddings` join).
    pub embeddable_notes_without_vector: u64,
}

/// Three-level drift scan: strict size → prefix-4 KB → full SHA-256.
///
/// Loads all `file_checksums` entries from `index`, then verifies each file
/// via `storage` (relative paths).
///
/// ## OpenDAL data path
///
/// `storage` is rooted at `vault_root`. The `relative_path` values from
/// `file_checksums` entries are directly usable as relative Storage paths.
/// - `stat(relative_path)` → on-disk size (equivalent to `fs::metadata`)
/// - `read(relative_path)` → full bytes (equivalent to `fs::read`)
///
/// ## Errors
///
/// Returns `GradatumError::Storage` if reading checksums fails.
/// Returns `GradatumError::Storage` if reading an existing file fails
/// (permissions, filesystem error). Missing files are collected in
/// `DriftScanResult::missing`, not reported as errors.
#[must_use = "DriftScanResult contient les informations de drift — ne pas ignorer"]
pub async fn scan_phase_a(
    storage: &dyn Storage,
    index: &SqliteIndex,
) -> Result<DriftScanResult, GradatumError> {
    let entries = index.list_file_checksums().await?;
    let mut result = DriftScanResult::default();

    // Ensemble des chemins de notes suivis par `file_checksums` — sert à la direction
    // disque → index (geste 3). On ne retient que les entrées `Note` : les `.md` sur
    // disque sont des notes, jamais des overrides/config.
    let tracked_notes: HashSet<&str> = entries
        .iter()
        .filter(|e| e.file_kind == FileKind::Note)
        .map(|e| e.relative_path.as_str())
        .collect();

    for entry in &entries {
        let rel = &entry.relative_path;

        // Niveau 0 — existence : stat NotFound → fichier manquant.
        let meta = match storage.stat(rel).await {
            Ok(m) => m,
            Err(StorageError::NotFound(_)) => {
                // Fichier référencé dans file_checksums mais absent sur disque.
                // On conserve un PathBuf pour compatibilité avec DriftScanResult::missing.
                result.missing.push(PathBuf::from(rel));
                continue;
            }
            Err(e) => {
                return Err(GradatumError::Storage(format!(
                    "stat drift entry '{}': {e}",
                    rel
                )));
            }
        };

        let on_disk_size = meta.size;

        if on_disk_size == entry.expected_size {
            // Niveau 2 : prefix-4KB hash. ⚠️ `Storage::read` lit le fichier ENTIER —
            // seul le HASH porte sur les 4096 premiers bytes, pas la lecture. Le gain
            // du niveau 2 est donc le coût du hash, pas celui de l'I/O.
            let bytes = storage
                .read(rel)
                .await
                .map_err(|e| GradatumError::Storage(format!("read drift entry '{}': {e}", rel)))?;
            let prefix = compute_prefix_4kb_bytes(&bytes);
            if prefix == entry.expected_hash_prefix_4kb {
                // Fichier probablement inchangé — court-circuit
                result.level2_prefix_match += 1;
                continue;
            }
            // Prefix diffère malgré size identique → niveau 3 full hash (bytes déjà en mémoire)
            let full = compute_full_sha256_bytes(&bytes);
            if full == entry.expected_hash {
                result.level3_full_hash_match += 1;
            } else {
                result.level3_full_hash_mismatch += 1;
            }
        } else {
            // Size diffère → niveau 3 full hash
            let bytes = storage
                .read(rel)
                .await
                .map_err(|e| GradatumError::Storage(format!("read drift entry '{}': {e}", rel)))?;
            let full = compute_full_sha256_bytes(&bytes);
            if full == entry.expected_hash {
                result.level3_full_hash_match += 1;
            } else {
                result.level3_full_hash_mismatch += 1;
            }
        }
    }

    // ── Geste 3 (F-174) — direction disque → index : fichiers non suivis ──────────
    // Énumère le disque via `Storage::list` (récursif) et retient les `.md` de note
    // absents de `file_checksums`. Ferme l'angle mort structurel de la boucle ci-dessus.
    result.untracked = scan_untracked(storage, &tracked_notes).await?;

    // ── Geste 4 (F-174) — dimension vecteur : notes embeddables sans embedding ────
    result.embeddable_notes_without_vector =
        index.count_embeddable_notes_without_embedding().await?;

    Ok(result)
}

/// Lists note `.md` files on disk that are **absent** from `tracked`.
///
/// Walks `storage` recursively (`list("")`). A path is an *untracked note* when all hold:
/// - it is a file (not a directory),
/// - its extension is `.md`,
/// - its file stem parses as a ULID (a `README.md` is not a note),
/// - no path component is hidden (`.history/`, `.archive/`, `.gradatum/` are pruned —
///   `.history/` snapshots are not tracked notes and must never be flagged),
/// - its relative path is not in `tracked`.
///
/// Mirrors the exclusions of the `reindex-orphans` disk scanner so the two agree on what
/// counts as a note.
///
/// ## Symlinks
///
/// This scan is **read-only and observational**: it only *flags* a path, never reads or
/// writes based on it. The `Storage` layer already rejects `..` traversal
/// (`validate_relative_path`). A symlinked `.md` inside the vault would at worst be reported
/// as a spurious `untracked` entry — never a write outside the tree. That is why it does not
/// need the `follow_links(false)` that the `backfill-checksums` **writer** enforces: the
/// writer hashes bytes and writes a footprint keyed by path, where following a link could
/// footprint a file outside the tenant; the reader has no such exposure. (The live `main`
/// vault is a flat tree of real files, so the point is moot there.)
///
/// ## Errors
///
/// Returns `GradatumError::Storage` if the storage listing fails.
async fn scan_untracked(
    storage: &dyn Storage,
    tracked: &HashSet<&str>,
) -> Result<Vec<PathBuf>, GradatumError> {
    let listed = storage
        .list("")
        .await
        .map_err(|e| GradatumError::Storage(format!("list vault for untracked scan: {e}")))?;

    let mut untracked = Vec::new();
    for entry in listed {
        if entry.is_dir {
            continue;
        }
        if !is_note_md_path(&entry.path) {
            continue;
        }
        if !tracked.contains(entry.path.as_str()) {
            untracked.push(PathBuf::from(entry.path));
        }
    }
    // Déterminisme (utile aux tests et aux logs).
    untracked.sort();
    Ok(untracked)
}

/// `true` when `rel` is the relative path of a note `.md`: `.md` extension, ULID stem, and
/// no hidden path component. See [`scan_untracked`] for the rationale of each condition.
fn is_note_md_path(rel: &str) -> bool {
    // Aucun segment caché (`.history/`, `.archive/`, `.gradatum/`, …).
    if rel.split('/').any(|seg| seg.starts_with('.')) {
        return false;
    }
    let path = std::path::Path::new(rel);
    if path.extension().and_then(|e| e.to_str()) != Some("md") {
        return false;
    }
    match path.file_stem().and_then(|s| s.to_str()) {
        Some(stem) => Ulid::from_string(stem).is_ok(),
        None => false,
    }
}

/// Counts **embeddable** notes with no row in `note_embeddings`.
///
/// The status filter is derived from the SSOT
/// [`NoteStatus::embeddable_default_sql_list`](gradatum_core::status::NoteStatus::embeddable_default_sql_list)
/// — the exact set the embedding backfill repairs (`live`, `pending-review`, `staging`).
/// Detector and repairer MUST agree: a note the backfill would embed is a note this detector
/// flags when its vector is missing. Scoping narrower (e.g. `live` only) would leave a
/// `pending-review`/`staging` note repairable-but-unsignalled — the blind spot the drift scan
/// closes, re-introduced on the vector dimension. The embedding cost is not re-paid at
/// `pending-review → live`, so such a note never recovers its vector by going live.
///
/// The list is built from the enum's kebab representation (no user input) — interpolating it
/// into the SQL carries no injection risk.
///
/// Free function taking a `&Connection` so the query can be unit-tested against a minimal
/// schema, independently of the full migration chain.
///
/// ## Errors
///
/// Propagates any `rusqlite` error from the query.
pub(crate) fn count_embeddable_unembedded(conn: &rusqlite::Connection) -> rusqlite::Result<u64> {
    let status_list = gradatum_core::status::NoteStatus::embeddable_default_sql_list();
    let sql = format!(
        "SELECT count(*) FROM notes n \
         LEFT JOIN note_embeddings e ON n.id = e.note_id \
         WHERE e.note_id IS NULL AND n.status IN ({status_list})"
    );
    let n: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(n.max(0) as u64)
}

/// SHA-256 hash of the first 4 KB of `bytes`.
///
/// Operates in memory (bytes already loaded by `storage.read()`).
/// Consumes at most `min(bytes.len(), 4096)` bytes.
///
/// `pub`: the note write path (`gradatum-vault`) MUST hash a `file_checksums` entry
/// with exactly the same primitive as the one used here by `scan_phase_a` to compare.
/// Producer and consumer share this function to guarantee parity — a divergent hash
/// would make every file spuriously look "drifted". Do not duplicate on the consumer
/// side.
#[must_use]
pub fn compute_prefix_4kb_bytes(bytes: &[u8]) -> [u8; 32] {
    let prefix_len = bytes.len().min(4096);
    sha2::Sha256::digest(&bytes[..prefix_len]).into()
}

/// Full SHA-256 hash of `bytes`.
///
/// Suitable for Markdown notes typically under 1 MB.
/// Future evolution: streaming hash for large notes.
///
/// `pub`: see [`compute_prefix_4kb_bytes`] — producer/consumer parity of the drift
/// checksum.
#[must_use]
pub fn compute_full_sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}

/// Builds the `file_checksums` drift footprint for a note `.md` from its on-disk bytes.
///
/// Single source of the footprint **shape**, so that every producer records a checksum the
/// scanner ([`scan_phase_a`]) can compare byte-for-byte — a divergent footprint would make
/// files look spuriously drifted. Wraps the shared hash primitives and stamps
/// `expected_mtime`/`last_verified` with `now_secs` (the scan compares size + hash, not
/// mtime).
///
/// Consumed by the `backfill-checksums` admin command. The write funnel
/// (`gradatum-vault`) still assembles the same footprint inline; migrating it onto this
/// helper is a follow-up that touches that crate.
#[must_use]
pub fn build_note_checksum_entry(
    relative_path: String,
    bytes: &[u8],
    now_secs: i64,
) -> gradatum_core::index::FileChecksumEntry {
    gradatum_core::index::FileChecksumEntry {
        relative_path,
        file_kind: FileKind::Note,
        expected_size: bytes.len() as u64,
        expected_hash_prefix_4kb: compute_prefix_4kb_bytes(bytes),
        expected_hash: compute_full_sha256_bytes(bytes),
        expected_mtime: now_secs,
        last_verified: now_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_4kb_bytes_short_content() {
        // Vérifie que compute_prefix_4kb_bytes ne panique pas sur contenu < 4KB
        let hash = compute_prefix_4kb_bytes(b"hello");
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn full_sha256_bytes_empty() {
        let hash = compute_full_sha256_bytes(b"");
        // sha256("") = e3b0c44298fc1c149afb...
        assert_eq!(
            hash[0], 0xe3,
            "sha256 d'un contenu vide doit commencer par 0xe3"
        );
    }

    #[test]
    fn prefix_4kb_bytes_truncates_at_4096() {
        // Contenu de 5000 bytes → prefix = hash des 4096 premiers seulement
        let data = vec![0xABu8; 5000];
        let prefix = compute_prefix_4kb_bytes(&data);
        let full = compute_full_sha256_bytes(&data);
        // Les deux hashes doivent différer (contenus tronqués vs complets)
        assert_ne!(
            prefix, full,
            "prefix 4KB et full hash doivent différer pour >4KB"
        );
    }

    // ── Geste 3 (F-174) — classification d'un chemin de note ─────────────────────
    // Domaine où SEULE la règle de forme de chemin peut faire échouer.
    #[test]
    fn is_note_md_path_recognises_note_and_rejects_the_rest() {
        let id = ulid::Ulid::generate().to_string();
        // Une note : `<tenant>/<ulid>.md`, aucun segment caché.
        assert!(is_note_md_path(&format!("main/{id}.md")));
        // Note ARCHIVÉE : `main/.archive/<ulid>.md` — stem ULID VALIDE + extension `.md`.
        // SEUL le filtre de segment caché peut la rejeter (le filtre ULID la laisserait
        // passer) : domaine où la règle « caché » est seule à pouvoir déclencher.
        let archived = ulid::Ulid::generate().to_string();
        assert!(!is_note_md_path(&format!("main/.archive/{archived}.md")));
        // Fichier non-ULID : pas une note.
        assert!(!is_note_md_path("main/README.md"));
        // Mauvaise extension : pas une note.
        assert!(!is_note_md_path(&format!("main/{id}.txt")));
    }

    // ── Geste 4 (F-174) — note EMBEDDABLE sans vecteur ───────────────────────────
    // Schéma minimal isolé de la chaîne de migrations. Aucun nombre mesuré gravé :
    // on compte ce que le test fabrique lui-même. Le filtre du détecteur doit être
    // l'ensemble embeddable (live/pending-review/staging), pas seulement `live` — sinon
    // une note en revue sans vecteur est réparable mais jamais signalée (la réserve d'audit).
    #[test]
    fn count_embeddable_unembedded_counts_all_embeddable_statuses_without_vector() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE notes (id TEXT PRIMARY KEY, status TEXT NOT NULL);
             CREATE TABLE note_embeddings (note_id TEXT);",
        )
        .unwrap();

        let live_no_vec = ulid::Ulid::generate().to_string();
        let pending_no_vec = ulid::Ulid::generate().to_string();
        let staging_no_vec = ulid::Ulid::generate().to_string();
        let live_with_vec = ulid::Ulid::generate().to_string();
        let draft_no_vec = ulid::Ulid::generate().to_string();
        let deprecated_no_vec = ulid::Ulid::generate().to_string();

        for (id, status) in [
            (&live_no_vec, "live"),
            (&pending_no_vec, "pending-review"),
            (&staging_no_vec, "staging"),
            (&live_with_vec, "live"),
            (&draft_no_vec, "draft"),
            (&deprecated_no_vec, "deprecated"),
        ] {
            conn.execute(
                "INSERT INTO notes (id, status) VALUES (?1, ?2)",
                rusqlite::params![id, status],
            )
            .unwrap();
        }
        // Seule `live_with_vec` a un vecteur.
        conn.execute(
            "INSERT INTO note_embeddings (note_id) VALUES (?1)",
            rusqlite::params![live_with_vec],
        )
        .unwrap();

        // Comptées : les TROIS statuts embeddables sans vecteur (live, pending-review,
        // staging). Une note en revue (`pending-review`) sans vecteur DOIT être comptée —
        // c'est la propriété que la réserve d'audit verrouille. Exclues :
        // - `live_with_vec` (a un vecteur),
        // - `draft_no_vec` / `deprecated_no_vec` (non embeddables — domaine où la règle
        //   ne s'applique pas ; seul leur statut les distingue des trois retenues).
        let n = count_embeddable_unembedded(&conn).unwrap();
        assert_eq!(
            n, 3,
            "les 3 statuts embeddables sans vecteur doivent être comptés (dont pending-review)"
        );
    }

    // ── Scan complet (F-174) — les deux directions + la dimension vecteur ────────
    // Frontière : `scan_phase_a` doit remonter simultanément untracked (disque→index)
    // ET live-sans-vecteur, en plus du missing (index→disque) préexistant.
    #[tokio::test]
    async fn scan_phase_a_surfaces_untracked_and_missing_vector() {
        use gradatum_storage::FileStorage;

        let tmp = tempfile::tempdir().unwrap();
        let vault_root = tmp.path().join("vault");
        std::fs::create_dir_all(vault_root.join("main")).unwrap();
        let index_path = tmp.path().join("index.db");

        let idx = SqliteIndex::open(&index_path).await.unwrap();

        // (a) Note SUIVIE : présente sur disque ET dans file_checksums → ni untracked ni missing.
        let tracked_id = ulid::Ulid::generate().to_string();
        let tracked_rel = format!("main/{tracked_id}.md");
        let tracked_bytes = b"---\nvault_id: main\n---\n\n# Suivie\n";
        std::fs::write(vault_root.join(&tracked_rel), tracked_bytes).unwrap();
        idx.upsert_file_checksum(&gradatum_core::index::FileChecksumEntry {
            relative_path: tracked_rel.clone(),
            file_kind: FileKind::Note,
            expected_size: tracked_bytes.len() as u64,
            expected_hash_prefix_4kb: compute_prefix_4kb_bytes(tracked_bytes),
            expected_hash: compute_full_sha256_bytes(tracked_bytes),
            expected_mtime: 0,
            last_verified: 0,
        })
        .await
        .unwrap();

        // (b) Note NON SUIVIE : sur disque, absente de file_checksums → untracked (geste 3).
        let untracked_id = ulid::Ulid::generate().to_string();
        let untracked_rel = format!("main/{untracked_id}.md");
        std::fs::write(vault_root.join(&untracked_rel), b"# Non suivie\n").unwrap();

        // (c) Note ARCHIVÉE sous `.archive/` : stem ULID VALIDE — SEUL le pruning des
        // segments cachés l'exclut (sinon on la remonterait comme untracked et on
        // ressusciterait un archivé). Domaine où la règle « caché » est seule à trancher.
        let archived_id = ulid::Ulid::generate().to_string();
        std::fs::create_dir_all(vault_root.join("main/.archive")).unwrap();
        std::fs::write(
            vault_root.join(format!("main/.archive/{archived_id}.md")),
            b"# archived\n",
        )
        .unwrap();

        // (d) Note live SANS vecteur : insérée à même l'index → embeddable_notes_without_vector.
        let live_id = ulid::Ulid::generate().to_string();
        {
            let raw = rusqlite::Connection::open(&index_path).unwrap();
            raw.execute(
                "INSERT INTO notes \
                 (id, vault_id, section, status, schema_version, created, content_hash, body_text) \
                 VALUES (?1, 'main', 'decisions', 'live', 1, 0, ?2, '')",
                rusqlite::params![live_id, vec![0u8; 32]],
            )
            .unwrap();
        }

        let storage = FileStorage::new(&vault_root).unwrap();
        let result = scan_phase_a(&storage, &idx).await.unwrap();

        // Geste 3 : l'untracked est vu, le tracked et le snapshot caché ne le sont pas.
        let untracked: HashSet<String> = result
            .untracked
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            untracked.contains(&untracked_rel),
            "la note sur disque absente de file_checksums doit être untracked"
        );
        assert!(
            !untracked.contains(&tracked_rel),
            "une note suivie ne doit jamais être untracked"
        );
        assert!(
            untracked.iter().all(|p| !p.contains("/.archive/")),
            "une note archivée (segment caché) ne doit jamais être untracked"
        );

        // Geste 4 : la note live sans vecteur est comptée.
        assert_eq!(
            result.embeddable_notes_without_vector, 1,
            "la note live sans embedding doit être comptée"
        );

        // Direction préexistante (index→disque) : le tracked existe → 0 missing.
        assert!(
            result.missing.is_empty(),
            "aucune entrée file_checksums sans fichier"
        );
    }

    // ── Parité (F-174 geste 2) — l'empreinte du helper est lue « stable » par le scan ─
    // Une empreinte construite par `build_note_checksum_entry` sur les octets réels du
    // fichier DOIT être vue stable (level2), jamais dérivée : sans cette parité, un
    // rétro-remplissage rendrait chaque fichier faussement « dérivé ».
    #[tokio::test]
    async fn built_footprint_is_seen_stable_by_the_scanner() {
        use gradatum_storage::FileStorage;

        let tmp = tempfile::tempdir().unwrap();
        let vault_root = tmp.path().join("vault");
        std::fs::create_dir_all(vault_root.join("main")).unwrap();
        let index_path = tmp.path().join("index.db");
        let idx = SqliteIndex::open(&index_path).await.unwrap();

        let id = ulid::Ulid::generate().to_string();
        let rel = format!("main/{id}.md");
        let bytes = b"---\nvault_id: main\n---\n\n# Retro-remplie\n";
        std::fs::write(vault_root.join(&rel), bytes).unwrap();

        // Empreinte via le helper, puis upsert — exactement le geste du backfill.
        idx.upsert_file_checksum(&build_note_checksum_entry(rel.clone(), bytes, 0))
            .await
            .unwrap();

        let storage = FileStorage::new(&vault_root).unwrap();
        let result = scan_phase_a(&storage, &idx).await.unwrap();

        assert_eq!(
            result.level2_prefix_match, 1,
            "le fichier doit être vu stable (size+prefix identiques)"
        );
        assert_eq!(
            result.level3_full_hash_mismatch, 0,
            "aucune dérive : l'empreinte correspond aux octets"
        );
        assert!(result.missing.is_empty(), "le fichier existe → 0 missing");
        // Suivi désormais → plus untracked.
        assert!(
            !result.untracked.iter().any(|p| p.to_string_lossy() == rel),
            "une note dotée d'une empreinte n'est plus untracked"
        );
    }
}
