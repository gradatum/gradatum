//! Tests d'intégration — sub-commande `backfill-titles`.
//!
//! Stratégie :
//! - Créer une arborescence TempDir mimant le layout standard.
//! - Ouvrir `SqliteIndex::open(index_path)` pour appliquer les migrations schéma.
//! - Insérer des notes directement via rusqlite (INSERT minimal).
//! - Forcer `title = NULL` via UPDATE direct (simule l'état pré-migration 0005).
//! - Appeler `backfill_titles()` et vérifier les reports + état DB.

use gradatum_admin::BackfillTitlesArgs;
use gradatum_index::SqliteIndex;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers de test
// ─────────────────────────────────────────────────────────────────────────────

/// Crée l'arborescence TempDir minimale mimant `/var/lib/gradatum` :
///   - `vault/.gradatum/index.db` (créé par SqliteIndex::open pour appliquer les migrations)
///
/// Retourne `(root, TempDir)` — TempDir doit rester alive le temps du test.
async fn setup_root() -> (std::path::PathBuf, TempDir) {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().to_path_buf();

    std::fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");

    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open pour init schéma");

    (root, tmp)
}

/// Insère une note minimale dans l'index via rusqlite direct.
///
/// Insère avec un title calculé, puis force
/// `title = NULL` pour simuler l'état pré-migration 0005.
fn insert_note_null_title(
    conn: &rusqlite::Connection,
    note_id: &str,
    vault_id: &str,
    body_text: &str,
) {
    let hash = vec![0u8; 32];
    conn.execute(
        "INSERT INTO notes (
            id, vault_id, locus, section, status, schema_version,
            author_kind, author_id, author_display_name,
            created, updated, status_changed, status_reason,
            content_hash, version, body_text, integrity_signature, extra_json, tags
        ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, NULL, NULL, NULL, ?6, NULL, NULL, NULL, ?7, ?8, ?9, NULL, NULL, NULL)",
        rusqlite::params![
            note_id,
            vault_id,
            "reference",           // section
            "indexed",             // status
            1i64,                  // schema_version
            1_700_000_000_000i64,  // created (epoch ms)
            hash,
            1i64,                  // version
            body_text,
        ],
    )
    .expect("INSERT note test");

    // Force title = NULL pour simuler l'état pré-migration 0005.
    conn.execute(
        "UPDATE notes SET title = NULL WHERE id = ?1",
        rusqlite::params![note_id],
    )
    .expect("UPDATE title = NULL");
}

/// Lit le titre d'une note depuis la DB.
fn fetch_title(conn: &rusqlite::Connection, note_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT title FROM notes WHERE id = ?1",
        rusqlite::params![note_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .expect("SELECT title")
}

/// Construit les args pour backfill_titles sur la root donnée.
fn make_args(root: &std::path::Path, dry_run: bool) -> BackfillTitlesArgs {
    BackfillTitlesArgs {
        root: root.to_path_buf(),
        tenant: "main".to_string(),
        dry_run,
        limit: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests TDD C7-bis
// ─────────────────────────────────────────────────────────────────────────────

/// C7-bis — backfill 10 notes sans titre → 10 H1 extraits + persistés.
#[tokio::test]
async fn backfill_titles_extracts_h1_and_updates() {
    let (root, _tmp) = setup_root().await;
    let index_path = root.join("vault/.gradatum/index.db");

    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index");
        for i in 0..10usize {
            let id = format!("01TEST{i:04}");
            let body = format!("# Titre Note {i}\n\nCorps de la note {i}.");
            insert_note_null_title(&conn, &id, "main", &body);
        }
    }

    let report = gradatum_admin::backfill_titles(make_args(&root, false))
        .await
        .expect("backfill_titles");

    assert_eq!(report.notes_scanned, 10, "10 notes scannées");
    assert_eq!(report.titles_extracted, 10, "10 H1 extraits");
    assert_eq!(report.titles_updated, 10, "10 titres mis à jour");
    assert_eq!(report.titles_no_h1, 0, "aucune note sans H1");

    // Vérifier la persistance en DB.
    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index post-backfill");
        for i in 0..10usize {
            let id = format!("01TEST{i:04}");
            let title = fetch_title(&conn, &id);
            assert_eq!(
                title.as_deref(),
                Some(format!("Titre Note {i}").as_str()),
                "titre persisté note {i}"
            );
        }
    }
}

/// C7-bis — idempotence : 2 runs successifs = même résultat, pas de re-écriture.
#[tokio::test]
async fn backfill_titles_idempotent_two_runs() {
    let (root, _tmp) = setup_root().await;
    let index_path = root.join("vault/.gradatum/index.db");

    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index");
        for i in 0..5usize {
            let id = format!("01IDEM{i:04}");
            let body = format!("# Idempotent {i}\n\nCorps.");
            insert_note_null_title(&conn, &id, "main", &body);
        }
    }

    // Run 1.
    let report1 = gradatum_admin::backfill_titles(make_args(&root, false))
        .await
        .expect("backfill run 1");
    assert_eq!(report1.titles_updated, 5, "run 1 : 5 titres mis à jour");

    // Run 2 : title IS NULL → 0 notes sélectionnées.
    let report2 = gradatum_admin::backfill_titles(make_args(&root, false))
        .await
        .expect("backfill run 2");
    assert_eq!(
        report2.notes_scanned, 0,
        "run 2 idempotent : 0 notes sans titre"
    );
    assert_eq!(
        report2.titles_updated, 0,
        "run 2 idempotent : 0 mises à jour"
    );
}

/// C7-bis — note sans H1 : ignorée silencieusement, comptée dans titles_no_h1.
#[tokio::test]
async fn backfill_titles_notes_without_h1_are_skipped() {
    let (root, _tmp) = setup_root().await;
    let index_path = root.join("vault/.gradatum/index.db");

    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index");
        // Note avec H2 seulement (pas de H1).
        insert_note_null_title(&conn, "01NOHEA1", "main", "## Titre H2\n\nCorps sans H1.");
        // Note sans aucun titre.
        insert_note_null_title(&conn, "01NOHEA2", "main", "Corps sans aucun titre.");
    }

    let report = gradatum_admin::backfill_titles(make_args(&root, false))
        .await
        .expect("backfill_titles sans H1");

    assert_eq!(report.notes_scanned, 2, "2 notes scannées");
    assert_eq!(report.titles_updated, 0, "0 mises à jour : pas de H1");
    assert_eq!(report.titles_no_h1, 2, "2 notes sans H1 comptabilisées");

    // Les notes doivent toujours avoir title = NULL en DB.
    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index post-backfill");
        assert!(
            fetch_title(&conn, "01NOHEA1").is_none(),
            "title reste NULL pour note sans H1"
        );
        assert!(
            fetch_title(&conn, "01NOHEA2").is_none(),
            "title reste NULL pour note sans H1"
        );
    }
}

/// C7-bis — dry-run : aucune modification en DB, report indique les titres
/// qui auraient été mis à jour.
#[tokio::test]
async fn backfill_titles_dry_run_does_not_write() {
    let (root, _tmp) = setup_root().await;
    let index_path = root.join("vault/.gradatum/index.db");

    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index");
        for i in 0..3usize {
            let id = format!("01DRYRN{i:04}");
            let body = format!("# Dry Run Note {i}\n\nCorps.");
            insert_note_null_title(&conn, &id, "main", &body);
        }
    }

    let report = gradatum_admin::backfill_titles(make_args(&root, true))
        .await
        .expect("backfill dry-run");

    assert_eq!(report.notes_scanned, 3, "3 notes scannées en dry-run");
    assert_eq!(report.titles_extracted, 3, "3 H1 extraits en dry-run");
    assert_eq!(report.titles_updated, 0, "dry-run : aucune écriture en DB");

    // Vérifier que la DB est inchangée.
    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index post dry-run");
        for i in 0..3usize {
            let id = format!("01DRYRN{i:04}");
            assert!(
                fetch_title(&conn, &id).is_none(),
                "title reste NULL après dry-run pour note {i}"
            );
        }
    }
}

/// C7-bis — H1 vide (`# ` suivi d'un espace seul) : ne doit PAS écrire
/// `title = ""`.
///
/// Avant SSOT 2026-06-14 : `extract_h1_title("# ")` retournait `Some("")` et la
/// garde `!is_empty()` de backfill_titles assurait le skip. Depuis l'alignement
/// SSOT `gradatum-index`, la fonction retourne directement `None` pour les H1 vides
/// — la garde est supprimée car filtrée en amont. Le comportement observable reste
/// identique : `title` reste `NULL`, `titles_no_h1 += 1`.
#[tokio::test]
async fn backfill_titles_empty_h1_is_skipped() {
    let (root, _tmp) = setup_root().await;
    let index_path = root.join("vault/.gradatum/index.db");

    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index");
        // H1 vide : "# " → extract_h1_title retourne None (SSOT 2026-06-14).
        insert_note_null_title(
            &conn,
            "01EMPTYH",
            "main",
            "# \nContenu sans titre H1 valide.",
        );
    }

    let report = gradatum_admin::backfill_titles(make_args(&root, false))
        .await
        .expect("backfill H1 vide");

    assert_eq!(
        report.titles_no_h1, 1,
        "H1 vide comptabilisé dans titles_no_h1"
    );
    assert_eq!(report.titles_updated, 0, "Aucun UPDATE pour H1 vide");

    // Le title doit rester NULL (pas "").
    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index post-backfill");
        let title = fetch_title(&conn, "01EMPTYH");
        assert!(
            title.is_none(),
            "title reste NULL après backfill sur H1 vide — reçu: {:?}",
            title
        );
    }
}

/// C7-bis — mélange : notes avec H1 + notes sans H1 + note déjà titrée.
///
/// Vérifie que la sélection WHERE title IS NULL est précise et que les notes
/// déjà titrées ne sont pas retraitées.
#[tokio::test]
async fn backfill_titles_mixed_notes() {
    let (root, _tmp) = setup_root().await;
    let index_path = root.join("vault/.gradatum/index.db");

    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index");

        // Note avec H1, title = NULL → doit être mise à jour.
        insert_note_null_title(&conn, "01MIXHA", "main", "# Titre A\n\nCorps A.");
        // Note sans H1, title = NULL → skip (titles_no_h1).
        insert_note_null_title(&conn, "01MIXHB", "main", "Pas de titre.\nCorps B.");
        // Note avec H1, title déjà rempli → hors sélection WHERE title IS NULL.
        insert_note_null_title(&conn, "01MIXHC", "main", "# Titre C\n\nCorps C.");
        conn.execute(
            "UPDATE notes SET title = 'Titre C existant' WHERE id = '01MIXHC'",
            [],
        )
        .expect("UPDATE title existant");
    }

    let report = gradatum_admin::backfill_titles(make_args(&root, false))
        .await
        .expect("backfill mixte");

    // Seules 2 notes sont SELECT (01MIXHA + 01MIXHB — 01MIXHC a déjà un titre).
    assert_eq!(report.notes_scanned, 2, "2 notes sans titre scannées");
    assert_eq!(report.titles_updated, 1, "1 titre mis à jour (01MIXHA)");
    assert_eq!(report.titles_no_h1, 1, "1 note sans H1 (01MIXHB)");

    {
        let conn = rusqlite::Connection::open(&index_path).expect("open index post-backfill");
        assert_eq!(
            fetch_title(&conn, "01MIXHA").as_deref(),
            Some("Titre A"),
            "01MIXHA mis à jour"
        );
        assert!(
            fetch_title(&conn, "01MIXHB").is_none(),
            "01MIXHB title reste NULL"
        );
        assert_eq!(
            fetch_title(&conn, "01MIXHC").as_deref(),
            Some("Titre C existant"),
            "01MIXHC inchangé"
        );
    }
}
