//! `gradatum-admin backfill-note-links` sub-command.
//!
//! Iterates over notes with `status = 'live'` that contain at least one `[[` wikilink but
//! have no outgoing edge in `note_links`, resolves their links through
//! [`gradatum_curator::wikilinks_sync::resolve_wikilinks_sync`], and inserts the missing
//! edges.
//!
//! ## Usage
//! ```text
//! gradatum-admin backfill-note-links --root /var/lib/gradatum --dry-run
//! gradatum-admin backfill-note-links --root /var/lib/gradatum --tenant main --limit 500
//! ```
//!
//! ## Idempotence
//!
//! Edges are written with `INSERT OR IGNORE`, so re-running against an already processed
//! database changes nothing. The scan itself skips any note that already has at least one
//! outgoing edge, and therefore returns nothing once every note has been processed.
//!
//! ## Known limitations
//!
//! - The scan selects notes with **zero** outgoing edges. This is a one-shot repair tool,
//!   not an incremental reconciler: a note edited after a first back-fill is not picked up
//!   again, because it already has edges and the scan's `LEFT JOIN` filters it out.
//! - `--tenant` selects the vault the back-fill runs against. On a deployment where
//!   multi-tenancy is disabled, the only meaningful value is `main`; the flag is kept for
//!   consistency with the other admin sub-commands.

use anyhow::{Context, Result};
use gradatum_core::paths::vault_index_path;
use std::path::PathBuf;

/// Escapes the SQLite `LIKE` metacharacters `%`, `_` and `\`, so a title containing them
/// cannot produce false positives in a `LIKE … ESCAPE '\\'` lookup.
///
/// Behaviour matches `gradatum_index::queries::escape_like_pattern` exactly; it is
/// duplicated here because `gradatum-index` does not export it publicly. Shared within the
/// crate (F-147: reused by [`crate::repair_note_links`]) to avoid a second copy.
///
/// ```text
/// escape_like_pattern("User%")   → "User\\%"
/// escape_like_pattern("Note_1")  → "Note\\_1"
/// escape_like_pattern("a\\b")    → "a\\\\b"
/// escape_like_pattern("Normal")  → "Normal"
/// ```
pub(crate) fn escape_like_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Arguments for the `backfill-note-links` sub-command.
#[derive(Debug, Clone)]
pub struct BackfillNoteLinksArgs {
    /// Gradatum root directory (e.g. `/var/lib/gradatum`).
    pub root: PathBuf,
    /// Target tenant (default: `"main"`).
    pub tenant: String,
    /// Dry-run mode: walks the candidate notes without writing anything.
    pub dry_run: bool,
    /// Maximum number of notes to process; unlimited when absent.
    pub limit: Option<usize>,
}

/// Report of a `note_links` back-fill run.
#[derive(Debug, Default, Clone)]
#[must_use]
pub struct BackfillNoteLinksReport {
    /// Number of candidate notes scanned.
    pub notes_scanned: usize,
    /// Total number of edges submitted to `INSERT OR IGNORE`.
    ///
    /// This counts attempts, not rows actually inserted: an edge that already existed is
    /// ignored by SQLite yet still counted here.
    pub edges_written: usize,
    /// Number of notes for which at least one edge was submitted.
    pub notes_touched: usize,
    /// `true` when the run was a dry-run.
    pub dry_run: bool,
}

/// Returns the notes of this vault that have `status = 'live'`, contain at least one `[[`
/// wikilink, and have no outgoing edge at all in `note_links`.
///
/// Idempotent: once every edge exists, the result is an empty `Vec`.
///
/// # Errors
///
/// The database is unreachable, or the query fails.
fn notes_needing_backfill(
    conn: &rusqlite::Connection,
    vault_id: &str,
    limit: Option<usize>,
) -> Result<Vec<(String, String)>> {
    let limit_clause = limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    let query = format!(
        "SELECT n.id, n.body_text \
         FROM notes n \
         LEFT JOIN note_links l ON l.src_note_id = n.id AND l.vault_id = n.vault_id \
         WHERE n.vault_id = ?1 \
           AND n.status = 'live' \
           AND n.body_text LIKE '%[[%' \
           AND l.src_note_id IS NULL \
         ORDER BY n.created ASC \
         {limit_clause}"
    );

    let mut stmt = conn
        .prepare(&query)
        .context("preparing SELECT notes without note_links")?;

    let rows: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![vault_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("executing SELECT notes without note_links")?
        .collect::<std::result::Result<_, _>>()
        .context("collecting notes without note_links")?;

    drop(stmt);
    Ok(rows)
}

/// Resolves the wikilinks of one note and inserts the edges that are missing.
///
/// Idempotent: edges are written with `INSERT OR IGNORE`, so an edge that already exists
/// is silently ignored rather than raising an error.
///
/// Returns the number of edges submitted to `INSERT OR IGNORE`. That is a count of
/// attempts, not of rows actually inserted: already-existing edges are included, because
/// the figure is meant for the run report.
///
/// # Errors
///
/// The database is unreachable, or an insert fails.
fn backfill_one(
    conn: &rusqlite::Connection,
    vault_id: &str,
    note_id: &str,
    body: &str,
) -> Result<usize> {
    let edges = gradatum_curator::wikilinks_sync::resolve_wikilinks_sync(
        vault_id,
        note_id,
        body,
        // id_lookup_fn : vérifie l'existence d'une note live par son ULID
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
        // title_lookup_fn : résolution H1 Markdown en parité avec
        // `gradatum_index::Index::title_lookup` (queries.rs L371-403).
        // Cherche `# {title}\n…` (H1 avec LF) OU `# {title}` (sans LF final),
        // en échappant les wildcards LIKE via `escape_like_pattern`.
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
    );

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut count = 0usize;

    for (src, dst) in &edges {
        conn.execute(
            "INSERT OR IGNORE INTO note_links \
             (src_note_id, dst_note_id, vault_id, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![src, dst, vault_id, now_ms],
        )
        .with_context(|| format!("INSERT note_links src={src} dst={dst}"))?;
        count += 1;
    }

    Ok(count)
}

/// Runs the full back-fill of missing `note_links` edges for one tenant.
///
/// In `dry_run` mode the candidate notes are walked but nothing is written.
///
/// # Errors
///
/// The database is unreachable, wikilink resolution fails fatally, or an insert raises a
/// non-ignorable error.
pub async fn run(args: BackfillNoteLinksArgs) -> Result<BackfillNoteLinksReport> {
    let db_path = vault_index_path(&args.root);

    if !db_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the server must have started at least once",
            db_path.display()
        );
    }

    tokio::task::spawn_blocking(move || {
        run_backfill_sync(&db_path, &args.tenant, args.dry_run, args.limit)
    })
    .await
    .context("spawn_blocking backfill_note_links")?
}

/// Synchronous back-fill implementation; called from `spawn_blocking`.
fn run_backfill_sync(
    db_path: &std::path::Path,
    tenant: &str,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<BackfillNoteLinksReport> {
    let conn =
        rusqlite::Connection::open(db_path).context("opening index.db for backfill-note-links")?;

    // WAL pragma : lecture/écriture concurrente avec gradatum-server.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .context("PRAGMA journal_mode=WAL")?;

    let candidates = notes_needing_backfill(&conn, tenant, limit)
        .context("scan notes candidates backfill-note-links")?;

    let notes_scanned = candidates.len();
    let mut edges_written = 0usize;
    let mut notes_touched = 0usize;

    if dry_run {
        // Dry-run : résoudre RÉELLEMENT les wikilinks (mêmes lookups DB qu'en mode
        // réel) mais sans insérer dans `note_links`.
        // `edges_written` = nombre d'arêtes qui auraient été écrites (résolues),
        // PAS le compte brut de wikilinks extraits — un lien non-résolvable ne compte
        // pas (parité avec le mode réel, cohérence des rapports).
        for (note_id, body) in &candidates {
            let resolved = gradatum_curator::wikilinks_sync::resolve_wikilinks_sync(
                tenant,
                note_id,
                body,
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
            );
            let count = resolved.len();
            if count > 0 {
                notes_touched += 1;
                edges_written += count;
            }
        }
        tracing::info!(
            notes_scanned,
            edges_written,
            notes_touched,
            "backfill-note-links DRY-RUN complete"
        );
        return Ok(BackfillNoteLinksReport {
            notes_scanned,
            edges_written,
            notes_touched,
            dry_run: true,
        });
    }

    // Mode réel : insérer les arêtes.
    for (note_id, body) in &candidates {
        let count = backfill_one(&conn, tenant, note_id, body)
            .with_context(|| format!("backfill_one note_id={note_id}"))?;
        edges_written += count;
        if count > 0 {
            notes_touched += 1;
        }
    }

    tracing::info!(
        notes_scanned,
        edges_written,
        notes_touched,
        "backfill-note-links complete"
    );

    Ok(BackfillNoteLinksReport {
        notes_scanned,
        edges_written,
        notes_touched,
        dry_run: false,
    })
}

// ── Helpers de test ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Crée le schéma minimal nécessaire aux tests (`notes` + `note_links`).
    fn create_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE notes (
                id        TEXT    PRIMARY KEY,
                vault_id  TEXT    NOT NULL,
                status    TEXT    NOT NULL,
                body_text TEXT    NOT NULL DEFAULT '',
                title     TEXT    NOT NULL DEFAULT '',
                created   INTEGER NOT NULL DEFAULT 0,
                updated   INTEGER NOT NULL DEFAULT 0
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

    // ── Task 2-b Scan ──────────────────────────────────────────────────────────

    /// `notes_needing_backfill` retourne uniquement les notes live avec wikilinks
    /// et SANS arête sortante existante.
    #[test]
    fn scan_returns_only_notes_with_wikilinks_and_no_edges() {
        let conn =
            rusqlite::Connection::open_in_memory().expect("DB in-memory — invariant de test");
        create_schema(&conn).expect("schéma test");

        // (a) note live avec wikilink, 0 arête → candidate
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "note-a",
                "main",
                "live",
                "Voir [[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K]]"
            ],
        )
        .expect("insert note-a");

        // (b) note live SANS wikilink → ne doit pas être sélectionnée
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["note-b", "main", "live", "Corps sans crochets"],
        )
        .expect("insert note-b");

        // (c) note live avec wikilink ET déjà une arête → ne doit pas être sélectionnée
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "note-c",
                "main",
                "live",
                "Lien [[project:gradatum]] existant"
            ],
        )
        .expect("insert note-c");
        conn.execute(
            "INSERT INTO note_links (src_note_id, dst_note_id, vault_id, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["note-c", "project:gradatum", "main", 0i64],
        )
        .expect("insert arête note-c");

        let result = notes_needing_backfill(&conn, "main", None).expect("scan OK");

        assert_eq!(
            result.len(),
            1,
            "seule note-a doit être candidate — result={result:?}"
        );
        assert_eq!(result[0].0, "note-a", "la candidate doit être note-a");
    }

    /// `notes_needing_backfill` respecte le `LIMIT` quand spécifié.
    #[test]
    fn scan_respects_limit() {
        let conn = rusqlite::Connection::open_in_memory().expect("DB in-memory");
        create_schema(&conn).expect("schéma test");

        for i in 0..5 {
            conn.execute(
                "INSERT INTO notes (id, vault_id, status, body_text, created) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    format!("note-{i}"),
                    "main",
                    "live",
                    format!("Voir [[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K_{i}]]"),
                    i as i64,
                ],
            )
            .expect("insert");
        }

        let result = notes_needing_backfill(&conn, "main", Some(2)).expect("scan OK");
        assert_eq!(result.len(), 2, "LIMIT 2 doit restreindre à 2 candidats");
    }

    // ── Task 2-b backfill_one ──────────────────────────────────────────────────

    /// `backfill_one` insère une arête synthétique pour un nœud réservé
    /// et est idempotent (2e appel = pas d'erreur, même résultat).
    #[test]
    fn backfill_one_inserts_reserved_node_edge_and_is_idempotent() {
        let conn = rusqlite::Connection::open_in_memory().expect("DB in-memory");
        create_schema(&conn).expect("schéma test");

        // Note live avec wikilink réservé
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["src-note", "main", "live", "Voir [[status:DONE]]"],
        )
        .expect("insert src-note");

        // Premier appel → doit insérer 1 arête
        let count1 = backfill_one(&conn, "main", "src-note", "Voir [[status:DONE]]")
            .expect("backfill_one appel 1");
        assert_eq!(count1, 1, "doit insérer 1 arête au 1er appel");

        // Vérifier que l'arête est en base
        let link_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM note_links WHERE src_note_id = 'src-note'",
                [],
                |r| r.get(0),
            )
            .expect("count note_links");
        assert_eq!(link_count, 1, "note_links doit contenir 1 arête");

        // Deuxième appel → idempotent, pas d'erreur
        let count2 = backfill_one(&conn, "main", "src-note", "Voir [[status:DONE]]")
            .expect("backfill_one appel 2 idempotent");
        assert_eq!(
            count2, 1,
            "le 2e appel tente toujours 1 INSERT (ignoré par OR IGNORE)"
        );

        // L'arête est toujours là et unique
        let link_count2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM note_links WHERE src_note_id = 'src-note'",
                [],
                |r| r.get(0),
            )
            .expect("count note_links après idempotence");
        assert_eq!(link_count2, 1, "toujours exactement 1 arête après 2 appels");
    }

    /// `backfill_one` résout un ULID via `id_lookup_fn` quand la note cible existe.
    #[test]
    fn backfill_one_resolves_ulid_when_target_note_exists() {
        let conn = rusqlite::Connection::open_in_memory().expect("DB in-memory");
        create_schema(&conn).expect("schéma test");

        // Note cible live
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text, title) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "01KVBTMYNK4XXZJAKWMTB4AM9K",
                "main",
                "live",
                "Corps de la cible",
                "Cible"
            ],
        )
        .expect("insert cible");

        // Note source avec wikilink ULID
        let body = "Voir [[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K]]";
        let count = backfill_one(&conn, "main", "src-note-ulid", body).expect("backfill_one ULID");
        assert_eq!(count, 1, "doit résoudre et insérer 1 arête ULID");
    }

    // ── Task 2-b run (async) ───────────────────────────────────────────────────

    /// Crée la DB dans le layout canonique `{root}/vault/.gradatum/index.db`.
    fn setup_db_in_layout(root: &std::path::Path) -> rusqlite::Connection {
        let vault_gradatum = root.join("vault").join(".gradatum");
        std::fs::create_dir_all(&vault_gradatum).expect("créer répertoire vault/.gradatum");
        let db_path = vault_gradatum.join("index.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open DB");
        create_schema(&conn).expect("schéma");
        conn
    }

    /// Ouvre la DB existante dans le layout canonique pour vérification.
    fn open_db(root: &std::path::Path) -> rusqlite::Connection {
        let db_path = root.join("vault").join(".gradatum").join("index.db");
        rusqlite::Connection::open(&db_path).expect("open DB vérif")
    }

    /// `run(dry_run=true)` → `edges_written > 0` (arêtes résolues, pas brutes), note_links vide.
    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());

        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["note-dry", "main", "live", "Voir [[status:DONE]]"],
        )
        .expect("insert note");
        drop(conn);

        let args = BackfillNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: true,
            limit: None,
        };

        let report = run(args).await.expect("run dry-run OK");

        assert!(report.dry_run, "report.dry_run doit être true");
        assert_eq!(report.notes_scanned, 1, "1 note scannée");

        // Vérifier que note_links reste vide
        let conn = open_db(dir.path());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM note_links", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 0, "note_links doit rester vide en dry-run");
    }

    /// `run(dry_run=false)` avec nœud réservé → arête insérée en base.
    #[tokio::test]
    async fn real_run_writes_edges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());

        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["note-real", "main", "live", "Voir [[status:DONE]]"],
        )
        .expect("insert note");
        drop(conn);

        let args = BackfillNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: false,
            limit: None,
        };

        let report = run(args).await.expect("run réel OK");

        assert!(!report.dry_run, "report.dry_run doit être false");
        assert_eq!(report.notes_scanned, 1, "1 note scannée");
        assert!(
            report.edges_written > 0,
            "doit avoir écrit au moins 1 arête"
        );
        assert_eq!(report.notes_touched, 1, "1 note touchée");

        // Vérifier en base
        let conn = open_db(dir.path());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM note_links", [], |r| r.get(0))
            .expect("count");
        assert!(count > 0, "note_links doit contenir au moins 1 arête");
    }

    // ── P1 : parité title_lookup ───────────────────────────────────────────────

    /// P1-parité : un lien `[[Architecture]]` résout vers une note dont le titre
    /// est en H1 (`# Architecture`) et PAS dans la colonne `title` (sparse).
    /// Vérifie que le backfill produit la même arête que le worker.
    #[test]
    fn title_lookup_resolves_h1_when_title_column_is_null() {
        let conn = rusqlite::Connection::open_in_memory().expect("DB in-memory");
        create_schema(&conn).expect("schéma test");

        // Note cible : titre en H1 dans body_text, colonne title vide (sparse).
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text, title) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "dst-arch",
                "main",
                "live",
                "# Architecture\nLe contenu de la note...",
                "" // title NULL-équivalent (colonne vide, non renseignée)
            ],
        )
        .expect("insert note cible H1");

        // Note source avec wikilink titre libre vers "Architecture".
        let src_id = "src-wiki-arch";
        let body = "Voir [[Architecture]] pour les détails.";

        let count = backfill_one(&conn, "main", src_id, body).expect("backfill_one title H1");

        assert_eq!(
            count, 1,
            "doit résoudre [[Architecture]] via H1 et insérer 1 arête"
        );

        let dst: String = conn
            .query_row(
                "SELECT dst_note_id FROM note_links WHERE src_note_id = ?1",
                rusqlite::params![src_id],
                |r| r.get(0),
            )
            .expect("arête présente");
        assert_eq!(
            dst, "dst-arch",
            "destination doit être dst-arch (résolution H1)"
        );
    }

    /// P1-escape : un titre contenant `%` ou `_` ne produit pas de faux positifs
    /// LIKE et échappe correctement via `escape_like_pattern`.
    #[test]
    fn title_lookup_escapes_like_wildcards() {
        let conn = rusqlite::Connection::open_in_memory().expect("DB in-memory");
        create_schema(&conn).expect("schéma test");

        // Note cible : titre avec underscore littéral.
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text, title) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "dst-note-v1",
                "main",
                "live",
                "# Note_v1\nCorps de la note.",
                ""
            ],
        )
        .expect("insert note cible Note_v1");

        // Note parasite qui matcherait si `_` n'est pas échappé.
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text, title) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "dst-note-x1",
                "main",
                "live",
                "# NoteXv1\nCorps parasite.",
                ""
            ],
        )
        .expect("insert note parasite NoteXv1");

        let src_id = "src-escape-test";
        let body = "Voir [[Note_v1]] pour les détails.";

        let count = backfill_one(&conn, "main", src_id, body).expect("backfill_one escape");

        assert_eq!(count, 1, "doit résoudre exactement Note_v1, pas NoteXv1");

        let dst: String = conn
            .query_row(
                "SELECT dst_note_id FROM note_links WHERE src_note_id = ?1",
                rusqlite::params![src_id],
                |r| r.get(0),
            )
            .expect("arête présente");
        assert_eq!(
            dst, "dst-note-v1",
            "doit pointer vers Note_v1, pas vers le parasite NoteXv1"
        );
    }

    // ── P2 : dry-run compte les arêtes résolues, pas brutes ───────────────────

    /// P2-parité : dry-run résout réellement les wikilinks et compte les arêtes
    /// résolues (pas les wikilinks bruts). Un lien non-résolvable ne gonfle pas
    /// le compteur.
    #[tokio::test]
    async fn dry_run_counts_resolved_edges_not_raw_wikilinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());

        // Note cible live (résolvable via nœud réservé `[[status:DONE]]`).
        // Note source : 2 wikilinks — 1 réservé (résolvable) + 1 titre inexistant
        // (non-résolvable) → dry-run doit compter 1 arête résolue, pas 2.
        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "note-p2-src",
                "main",
                "live",
                "Voir [[status:DONE]] et [[TitreInexistant]]."
            ],
        )
        .expect("insert note source P2");
        drop(conn);

        let args = BackfillNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: true,
            limit: None,
        };

        let report = run(args).await.expect("run dry-run P2 OK");

        assert!(report.dry_run, "report.dry_run doit être true");
        assert_eq!(report.notes_scanned, 1, "1 note scannée");
        assert_eq!(
            report.edges_written, 1,
            "edges_written == 1 (arêtes résolues) — [[TitreInexistant]] ne compte pas"
        );

        // Rien ne doit avoir été écrit en base.
        let check_conn = open_db(dir.path());
        let count: i64 = check_conn
            .query_row("SELECT COUNT(*) FROM note_links", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 0, "note_links doit rester vide en dry-run");
    }

    // ── Tests escape_like_pattern (unitaire) ───────────────────────────────────

    /// escape_like_pattern : vérification des 4 cas documentés.
    #[test]
    fn escape_like_pattern_handles_all_cases() {
        assert_eq!(escape_like_pattern("User%"), "User\\%");
        assert_eq!(escape_like_pattern("Note_1"), "Note\\_1");
        assert_eq!(escape_like_pattern("a\\b"), "a\\\\b");
        assert_eq!(escape_like_pattern("Normal"), "Normal");
    }

    /// Idempotence du `run` : 2e exécution → 0 notes scannées (tout déjà traité).
    #[tokio::test]
    async fn run_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = setup_db_in_layout(dir.path());

        conn.execute(
            "INSERT INTO notes (id, vault_id, status, body_text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["note-idem", "main", "live", "Voir [[status:DONE]]"],
        )
        .expect("insert note");
        drop(conn);

        let make_args = || BackfillNoteLinksArgs {
            root: dir.path().to_path_buf(),
            tenant: "main".to_string(),
            dry_run: false,
            limit: None,
        };

        let report1 = run(make_args()).await.expect("run 1 OK");
        assert_eq!(report1.notes_scanned, 1);

        // 2e run : la note a déjà des arêtes → non renvoyée par le scan
        let report2 = run(make_args()).await.expect("run 2 OK");
        assert_eq!(
            report2.notes_scanned, 0,
            "2e run ne doit trouver aucune candidate"
        );
        assert_eq!(report2.edges_written, 0);
    }
}
