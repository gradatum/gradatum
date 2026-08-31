//! Tests intégration `downgrade-from-legacy-vault-trash`.
//!
//! Stratégie :
//! - Créer une arborescence TempDir mimant le layout de production (profondeur 4).
//! - Structure `.vault-trash/<date>/dedup/<section>/<file>.md`.
//! - Initialiser le schéma DB via `SqliteIndex::open` + `create_queue_db` (layout production).
//! - Insérer des notes directement via rusqlite (INSERT minimal).
//! - Appeler `downgrade_from_vault_trash()` et vérifier stats + état DB.
//!
//! F-177 : la file legacy `SqliteQueue` (`jobs_v2`) est supprimée. `db/queue.sqlite`
//! est initialisé avec le schéma `jobs` de la file rusqlite — miroir de `gradatum-admin init`.

use gradatum_admin::{DowngradeFromTrashArgs, downgrade_from_vault_trash};
use gradatum_index::SqliteIndex;
use std::fs;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers

/// Insère une note minimale dans l'index via rusqlite direct.
///
/// `body_text` sera utilisé pour le match heuristique `substr(body_text, 1, 200)`.
/// Retourne l'`id` de la note insérée.
fn insert_note(index_path: &std::path::Path, body_text: &str, status: &str) -> String {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db");
    // Générer un ID ULID-like déterministe (timestamp + compteur monotone suffisant)
    let id = format!(
        "01{:018}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let hash = vec![0u8; 32];
    let now = chrono::Utc::now().timestamp_millis();

    conn.execute(
        "INSERT INTO notes (
            id, vault_id, locus, section, status, schema_version,
            author_kind, author_id, author_display_name,
            created, updated, status_changed, status_reason,
            content_hash, version, body_text, integrity_signature, extra_json, tags
        ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, NULL, NULL, NULL, ?6, NULL, NULL, NULL, ?7, ?8, ?9, NULL, NULL, NULL)",
        rusqlite::params![
            id,
            "main",          // vault_id
            "reference",     // section
            status,          // status
            1i64,            // schema_version
            now,             // created
            hash,            // content_hash
            1i64,            // version
            body_text,
        ],
    )
    .expect("INSERT note test");

    id
}

/// Crée un fichier `.md` dans la structure réelle à 4 niveaux :
///   `.vault-trash/<date>/dedup/<section>/<name>.md`
fn create_trash_md_deep(
    legacy_vault: &std::path::Path,
    date: &str,
    section: &str,
    name: &str,
    body: &str,
) {
    let dir = legacy_vault
        .join(".vault-trash")
        .join(date)
        .join("dedup")
        .join(section);
    fs::create_dir_all(&dir).expect("mkdir .vault-trash/date/dedup/section");
    fs::write(dir.join(format!("{name}.md")), body).expect("write trash .md");
}

/// Crée un fichier `.md` dans la structure legacy à 2 niveaux (pour tests mixed) :
///   `.vault-trash/<date>/<name>.md`
fn create_trash_md_legacy(legacy_vault: &std::path::Path, date: &str, name: &str, body: &str) {
    let dir = legacy_vault.join(".vault-trash").join(date);
    fs::create_dir_all(&dir).expect("mkdir .vault-trash/date");
    fs::write(dir.join(format!("{name}.md")), body).expect("write trash .md legacy");
}

/// Crée `db/queue.sqlite` avec le schéma `jobs` de la file rusqlite (LegacyQueue).
///
/// F-177 : `SqliteQueue` (`jobs_v2`) est supprimé ; `gradatum-admin init` crée
/// toujours `db/queue.sqlite` avec la table `jobs` — ce helper en est le miroir.
fn create_queue_db(root: &std::path::Path) {
    let queue_path = root.join("db/queue.sqlite");
    let conn = rusqlite::Connection::open(&queue_path).expect("open queue.sqlite");
    conn.execute_batch(gradatum_queue::schema::CREATE_JOBS_TABLE)
        .expect("create jobs table");
    conn.execute_batch(gradatum_queue::schema::CREATE_IDX_JOBS_STATUS_LEASE)
        .expect("create jobs index");
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests

/// Cas 1 : `.vault-trash` vide → stats toutes à zéro.
#[tokio::test]
async fn empty_trash_returns_zero_stats() {
    // vault isolé dans le même TempDir que gradatum-root pour garantir l'isolation
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);
    fs::create_dir_all(vault.join(".vault-trash")).expect("mkdir .vault-trash");

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root,
        dry_run: false,
        limit: None,
    };
    let stats = downgrade_from_vault_trash(args).await.expect("run ok");

    assert_eq!(stats.trash_files_scanned, 0, "aucun fichier scanné");
    assert_eq!(stats.downgraded, 0, "aucun downgrade");
    assert_eq!(stats.matched_in_gradatum, 0, "aucun match");
    assert_eq!(stats.not_matched, 0, "aucun not_matched");
}

/// Cas 2 : fichier `.md` dans trash (structure 4 niveaux) matche une note `live` → downgrade effectif.
///
/// Vérifie que :
/// - `stats.matched_in_gradatum == 1`
/// - `stats.downgraded == 1`
/// - la note en DB passe à `status='downgraded'`
#[tokio::test]
async fn match_and_downgrade_existing_note() {
    // vault isolé dans le même TempDir — évite pollution entre runs parallèles
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);

    // Body assez long (>200 chars) pour que l'heuristique substr 200 soit non-ambiguë
    let body = "Acme example agent runtime documentation primary embedded host infrastructure \
                Phase 2 avec déclencheurs Subagent-Driven Development pipeline et my-project \
                council obligatoire pour les choix transversaux sur services LIVE en production \
                gradatum legacy-vault mcp stub bridge 13 tools alpha.9 Phase 2.1.2.";

    let id = insert_note(&index_path, body, "live");
    // Structure réelle 4 niveaux : <date>/dedup/<section>/<file>.md
    create_trash_md_deep(&vault, "2026-04-01", "decisions", "test_note", body);

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root.clone(),
        dry_run: false,
        limit: None,
    };
    let stats = downgrade_from_vault_trash(args).await.expect("run ok");

    assert_eq!(stats.trash_files_scanned, 1, "1 fichier scanné");
    assert_eq!(stats.matched_in_gradatum, 1, "1 match : stats={stats:?}");
    assert_eq!(stats.downgraded, 1, "1 downgrade effectif");
    assert_eq!(stats.not_matched, 0, "0 not_matched");

    // Vérifier l'état en DB
    let conn = rusqlite::Connection::open(&index_path).expect("open index.db post");
    let status: String = conn
        .query_row(
            "SELECT status FROM notes WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .expect("SELECT status");
    assert_eq!(
        status, "downgraded",
        "note doit être downgraded post-migration"
    );
}

/// Cas 3 : dry-run → stats correctes, DB non modifiée.
///
/// Vérifie que le dry-run compte correctement le match mais ne touche pas la DB.
/// Structure 4 niveaux réelle.
#[tokio::test]
async fn dry_run_does_not_modify_db() {
    // vault isolé dans le même TempDir — évite pollution entre runs parallèles
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);

    let body = "Acme example agent runtime documentation primary embedded host infrastructure \
                Phase 2 avec déclencheurs Subagent-Driven Development pipeline et my-project \
                council obligatoire pour les choix transversaux sur services LIVE en production \
                gradatum legacy-vault mcp stub bridge 13 tools dry-run scenario alpha.9.";

    let id = insert_note(&index_path, body, "live");
    // Structure réelle 4 niveaux : <date>/dedup/<section>/<file>.md
    create_trash_md_deep(&vault, "2026-04-15", "reference", "test_note_dry", body);

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root.clone(),
        dry_run: true,
        limit: None,
    };
    let stats = downgrade_from_vault_trash(args)
        .await
        .expect("run dry-run ok");

    // En dry-run : downgraded est compté mais DB non modifiée
    assert_eq!(
        stats.downgraded, 1,
        "dry-run doit compter le downgrade : stats={stats:?}"
    );
    assert_eq!(stats.matched_in_gradatum, 1, "1 match en dry-run");

    // La DB ne doit PAS avoir été modifiée
    let conn = rusqlite::Connection::open(&index_path).expect("open index.db post dry-run");
    let status: String = conn
        .query_row(
            "SELECT status FROM notes WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .expect("SELECT status post dry-run");
    assert_eq!(
        status, "live",
        "dry-run ne modifie PAS la DB — statut doit rester 'live'"
    );
}

/// Cas 4 : scan profondeur 4 niveaux réel.
///
/// Vérifie que la structure `.vault-trash/<date>/dedup/<section>/note.md`
/// est bien scannée et produit `scanned > 0`.
/// C'était le bug #46 : la boucle 2-niveaux manquait ce cas.
#[tokio::test]
async fn test_scan_depth_4_levels_dedup_structure() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);

    let body = "Note de test profondeur 4 niveaux — structure dedup réelle \
                avec chemin vault-trash date dedup section fichier point md \
                pour valider le fix bug 46 scan récursif walkdir gradatum admin \
                downgrade-from-legacy-vault-trash Phase 2.2 patch 1 alpha 9.";

    let _id = insert_note(&index_path, body, "live");
    // Structure exacte constatée en production après dédup 2026-05-09
    create_trash_md_deep(&vault, "2026-05-09", "decisions", "01XYZ_dedup", body);

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root,
        dry_run: true, // dry-run : pas d'écriture DB, on vérifie juste le scan
        limit: None,
    };
    let stats = downgrade_from_vault_trash(args).await.expect("run ok");

    assert!(
        stats.trash_files_scanned >= 1,
        "bug #46 : doit scanner au moins 1 fichier en profondeur 4 — scanned={}",
        stats.trash_files_scanned
    );
    assert_eq!(
        stats.downgraded, 1,
        "1 match attendu en dry-run — stats={stats:?}"
    );
}

/// Cas 5 : structure mixte legacy 2-niveaux + nouveau 4-niveaux → scanne les deux.
///
/// Vérifie que WalkDir traverse aussi bien les fichiers à 2 niveaux (min_depth=2
/// suffit pour `.vault-trash/<date>/file.md`) que les 4 niveaux.
#[tokio::test]
async fn test_scan_depth_mixed_legacy_and_new() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);

    let body_legacy = "Note legacy structure un niveau dans date du vault-trash \
                       pour compatibilité ascendante scan récursif migration admin \
                       gradatum Phase 2.2 test mixed depth legacy path sans dedup \
                       sous-répertoire direct dans date répertoire legacy-vault trash.";

    let body_deep = "Note structure profonde quatre niveaux dedup section gradatum \
                     vault-trash nouveau format constaté après dédup mai 2026 en production \
                     decisions council retrospectives architecture reference embedded \
                     walkdir scan récursif max depth dix garde-fou anti-runaway ok.";

    let _id_legacy = insert_note(&index_path, body_legacy, "live");
    let _id_deep = insert_note(&index_path, body_deep, "live");

    // Legacy : <date>/<file>.md (profondeur 2 depuis .vault-trash)
    create_trash_md_legacy(&vault, "2026-03-01", "legacy_note", body_legacy);
    // Nouveau : <date>/dedup/<section>/<file>.md (profondeur 4 depuis .vault-trash)
    create_trash_md_deep(&vault, "2026-05-09", "council", "new_note", body_deep);

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root,
        dry_run: true,
        limit: None,
    };
    let stats = downgrade_from_vault_trash(args).await.expect("run ok");

    assert_eq!(
        stats.trash_files_scanned, 2,
        "doit scanner 2 fichiers (legacy + deep) — scanned={}",
        stats.trash_files_scanned
    );
    assert_eq!(
        stats.downgraded, 2,
        "2 matches attendus en dry-run — stats={stats:?}"
    );
}

/// Cas 6 : fichiers non-.md à différents niveaux sont ignorés.
///
/// Vérifie que `.json`, `.txt`, `.toml` présents dans l'arborescence
/// ne sont pas comptabilisés dans `trash_files_scanned`.
#[tokio::test]
async fn test_scan_with_other_extensions_ignored() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);

    // Créer le répertoire vault-trash avec des fichiers parasites
    let trash = vault.join(".vault-trash");
    let date_dir = trash.join("2026-05-09").join("dedup").join("reference");
    fs::create_dir_all(&date_dir).expect("mkdir date/dedup/reference");

    // Fichier .md valide
    fs::write(date_dir.join("valid.md"), "contenu valide").expect("write valid.md");
    // Fichiers parasites à ignorer
    fs::write(date_dir.join("metadata.json"), "{}").expect("write .json");
    fs::write(date_dir.join("README.txt"), "readme").expect("write .txt");
    // Parasite à la racine date/ (depth=2 depuis .vault-trash, mais extension non-md)
    fs::write(trash.join("2026-05-09").join("index.toml"), "[meta]")
        .expect("write .toml racine date");

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root,
        dry_run: true,
        limit: None,
    };
    let stats = downgrade_from_vault_trash(args).await.expect("run ok");

    assert_eq!(
        stats.trash_files_scanned, 1,
        "seul 1 fichier .md doit être compté — scanned={}",
        stats.trash_files_scanned
    );
    assert_eq!(
        stats.not_matched, 1,
        "le .md ne match rien en DB → not_matched=1"
    );
}

/// Cas 7 : dry-run avec structure 4 niveaux + DB SQLite test → log cohérent.
///
/// Vérifie la cohérence des stats avec plusieurs notes réparties
/// dans différentes sections de la structure dedup réelle.
#[tokio::test]
async fn test_dry_run_with_real_structure() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);

    let body_decisions = "Note decisions council Art 19 gradatum legacy-vault migration \
                          downgrade-from-legacy-vault-trash dry-run Phase 2.2 patch 1 test \
                          structure réelle quatre niveaux date dedup section fichier md \
                          walkdir récursif minDepth deux maxDepth dix garde-fou ok scan.";
    let body_reference = "Note reference architecture embedded backend hypervisor \
                          dry-run test quatre niveaux dedup section reference fichier md \
                          gradatum legacy-vault mcp stub bridge alpha 9 Phase 2.1.2 fix \
                          bug 46 backlog Phase 2.2 scan profondeur insuffisante résolu.";
    let body_unmatched = "Note sans correspondance dans gradatum pour test not_matched.";

    let _id1 = insert_note(&index_path, body_decisions, "live");
    let _id2 = insert_note(&index_path, body_reference, "live");
    // body_unmatched : pas inséré en DB → not_matched

    create_trash_md_deep(
        &vault,
        "2026-05-09",
        "decisions",
        "note_decisions",
        body_decisions,
    );
    create_trash_md_deep(
        &vault,
        "2026-05-09",
        "reference",
        "note_reference",
        body_reference,
    );
    create_trash_md_deep(
        &vault,
        "2026-05-09",
        "council",
        "note_unmatched",
        body_unmatched,
    );

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root,
        dry_run: true,
        limit: None,
    };
    let stats = downgrade_from_vault_trash(args).await.expect("run ok");

    assert_eq!(stats.trash_files_scanned, 3, "3 fichiers .md scannés");
    assert_eq!(stats.matched_in_gradatum, 2, "2 matches en DB");
    assert_eq!(stats.downgraded, 2, "2 dry-run downgrade comptés");
    assert_eq!(stats.not_matched, 1, "1 fichier sans correspondance DB");
    assert_eq!(stats.already_downgraded, 0, "0 déjà downgraded");
}

/// Cas 9 : `.vault-trash` absent → retour Ok avec stats à zéro (idempotent).
///
/// Vérifie que `.vault-trash` absent retourne Ok avec stats à zéro (idempotent).
/// Sens métier : "pas de trash dir = rien à migrer = OK".
#[tokio::test]
async fn test_run_with_no_vault_trash_dir_returns_empty_stats_ok() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);
    // legacy-vault existe mais SANS .vault-trash — situation post-cleanup
    fs::create_dir_all(&vault).expect("mkdir vault");
    // .vault-trash N'EST PAS créé intentionnellement

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root,
        dry_run: false,
        limit: None,
    };
    let result = downgrade_from_vault_trash(args).await;

    assert!(
        result.is_ok(),
        ".vault-trash absent doit retourner Ok, pas Err — got: {:?}",
        result.err()
    );
    let stats = result.unwrap();
    assert_eq!(stats.trash_files_scanned, 0, "scanned=0 si trash absent");
    assert_eq!(stats.matched_in_gradatum, 0, "matched=0 si trash absent");
    assert_eq!(
        stats.already_downgraded, 0,
        "already_downgraded=0 si trash absent"
    );
    assert_eq!(stats.downgraded, 0, "downgraded=0 si trash absent");
    assert_eq!(stats.not_matched, 0, "not_matched=0 si trash absent");
}

// ── Tests C1-C4 council backlog Phase 4 alpha.15 ────────────────────────────

/// R1/M1 — downgrade-from-legacy-vault-trash sur 10 fichiers : scanned=10, pas de panique.
///
/// Vérifie que le refactoring prepare-out-of-loop ne casse pas le comportement
/// observable : 10 fichiers `.md` scannés, comportement identique à avant.
#[tokio::test]
async fn downgrade_from_trash_ten_files_prepare_outside_loop() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    gradatum_index::SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);

    // Seed 10 notes en DB et 10 fichiers dans .vault-trash
    for i in 0..10usize {
        // Chaque body doit être unique pour éviter les faux-positifs de match
        let body = format!(
            "Note de test R1 prepare-out-of-loop numéro {} pour downgrade scan \
             dix fichiers Phase 4 alpha 15 council backlog fix gradatum admin \
             walkdir récursif structure quatre niveaux dedup section fichier md.",
            i
        );
        insert_note(&index_path, &body, "live");
        create_trash_md_deep(
            &vault,
            "2026-01-01",
            "reference",
            &format!("note_{i}"),
            &body,
        );
    }

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root,
        dry_run: false,
        limit: None,
    };
    // Ne doit pas paniquer — le stmt est préparé 1 fois avant la boucle
    let stats = downgrade_from_vault_trash(args)
        .await
        .expect("run ok — pas de panique prepare-out-of-loop");

    assert_eq!(
        stats.trash_files_scanned, 10,
        "10 fichiers doivent être scannés"
    );
    assert_eq!(stats.downgraded, 10, "10 notes downgraded");
    assert_eq!(stats.matched_in_gradatum, 10, "10 matches en DB");
}

/// R4 — fichier illisible (permissions 000) ne provoque pas de underflow du compteur.
///
/// Avant le fix R4, le compteur était incrémenté AVANT la tentative de lecture,
/// puis décrémenté si le fichier était illisible — risque de underflow si le
/// premier fichier était illisible (usize = 0 - 1 = overflow en release).
/// Après fix : l'incrément n'a lieu qu'APRÈS une lecture réussie.
#[tokio::test]
async fn downgrade_from_trash_unreadable_file_does_not_decrement_scanned() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    gradatum_index::SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);

    // Créer 2 fichiers : 1 illisible (permissions 000) + 1 lisible
    let trash_dir = vault
        .join(".vault-trash")
        .join("2026-01-01")
        .join("dedup")
        .join("reference");
    fs::create_dir_all(&trash_dir).expect("mkdir trash");

    // Fichier illisible
    let unreadable = trash_dir.join("unreadable.md");
    fs::write(&unreadable, "contenu illisible").expect("write unreadable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
            .expect("chmod 000 unreadable");
    }

    // Fichier lisible mais sans correspondance en DB
    let readable_body = "Note lisible sans correspondance DB pour test R4 decrement safe fix.";
    fs::write(trash_dir.join("readable.md"), readable_body).expect("write readable");

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root,
        dry_run: true,
        limit: None,
    };
    let stats = downgrade_from_vault_trash(args)
        .await
        .expect("run ok — pas de underflow");

    // Le fichier illisible ne doit PAS incrémenter trash_files_scanned
    // (comportement post-fix R4 : incrément après lecture réussie seulement)
    // Sur Linux root, les permissions 000 peuvent encore être lisibles — le test
    // vérifie uniquement que scanned ≤ 1 (pas de double-comptage) et pas de panique.
    assert!(
        stats.trash_files_scanned <= 1,
        "le fichier illisible ne doit pas être compté dans scanned — got={}",
        stats.trash_files_scanned
    );
}

/// Cas 8 : idempotence — note déjà downgraded dans DB → already_downgraded incrémenté.
///
/// Adapté à la structure 4 niveaux.
#[tokio::test]
async fn test_idempotent_already_downgraded() {
    let tmp = TempDir::new().expect("TempDir");
    let root = tmp.path().join("gradatum");
    let vault = tmp.path().join("legacy-vault");
    fs::create_dir_all(root.join("db")).expect("mkdir db");
    fs::create_dir_all(root.join("vault/.gradatum")).expect("mkdir vault/.gradatum");
    let index_path = root.join("vault/.gradatum/index.db");
    SqliteIndex::open(&index_path)
        .await
        .expect("SqliteIndex::open");
    create_queue_db(&root);

    let body = "Note déjà downgraded en DB pour test idempotence migrated from legacy-vault \
                vault-trash structure quatre niveaux dedup section fichier md gradatum \
                admin downgrade-from-legacy-vault-trash skip déjà downgraded alpha 9 fix \
                bug 46 walkdir récursif Phase 2.2 patch 1 idempotent re-run safe ok.";

    // Insérer la note avec status déjà 'downgraded'
    let id = insert_note(&index_path, body, "downgraded");
    // Structure 4 niveaux
    create_trash_md_deep(&vault, "2026-05-08", "retrospectives", "already_done", body);

    let args = DowngradeFromTrashArgs {
        legacy_vault_path: vault,
        gradatum_root: root.clone(),
        dry_run: false,
        limit: None,
    };
    let stats = downgrade_from_vault_trash(args).await.expect("run ok");

    assert_eq!(stats.trash_files_scanned, 1, "1 fichier scanné");
    assert_eq!(stats.matched_in_gradatum, 1, "1 match");
    assert_eq!(
        stats.already_downgraded, 1,
        "déjà downgraded → skip idempotent"
    );
    assert_eq!(stats.downgraded, 0, "0 downgrade effectif (déjà fait)");

    // Vérifier que la DB n'a pas été retouchée
    let conn = rusqlite::Connection::open(&index_path).expect("open index.db post");
    let status: String = conn
        .query_row(
            "SELECT status FROM notes WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .expect("SELECT status");
    assert_eq!(
        status, "downgraded",
        "statut downgraded inchangé après re-run"
    );
}
