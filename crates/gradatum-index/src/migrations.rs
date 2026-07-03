//! Embedded SQL migration runner.
//!
//! SQL files are included via `include_str!` at compile time.
//! The runner checks the `_schema_migrations` table (bootstrapped if absent)
//! and applies only versions not yet recorded.
//!
//! ## Note
//!
//! The `0001_phase1.sql` script itself contains the INSERT into `_schema_migrations`.
//! The runner must not re-insert that row — it simply checks whether the version
//! exists before executing the batch.

use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;

use gradatum_core::error::GradatumError;

/// Ordered list of migrations as `(version, sql)` pairs.
///
/// Order is law — never reorder or delete an existing entry.
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_phase1", include_str!("../migrations/0001_phase1.sql")),
    (
        "0002_wikilinks",
        include_str!("../migrations/0002_wikilinks.sql"),
    ),
    (
        "0003_add_tags_to_notes",
        include_str!("../migrations/0003_add_tags_to_notes.sql"),
    ),
    (
        "0004_vault_downgrade",
        include_str!("../migrations/0004_vault_downgrade.sql"),
    ),
    (
        "0005_add_title_column",
        include_str!("../migrations/0005_add_title_column.sql"),
    ),
    (
        "0006_event_log",
        include_str!("../migrations/0006_event_log.sql"),
    ),
    (
        "0007_event_log_agent_id",
        include_str!("../migrations/0007_event_log_agent_id.sql"),
    ),
    (
        "0008_note_cognitive_kind",
        include_str!("../migrations/0008_note_cognitive_kind.sql"),
    ),
    (
        "0009_backfill_title",
        include_str!("../migrations/0009_backfill_title.sql"),
    ),
    (
        "0010_provenance_trust_redirects",
        include_str!("../migrations/0010_provenance_trust_redirects.sql"),
    ),
    (
        "0011_council_section_backfill",
        include_str!("../migrations/0011_council_section_backfill.sql"),
    ),
    (
        "0012_forgotten_columns",
        include_str!("../migrations/0012_forgotten_columns.sql"),
    ),
    (
        "0013_temporal_index",
        include_str!("../migrations/0013_temporal_index.sql"),
    ),
    (
        "0014_event_log_outcome",
        include_str!("../migrations/0014_event_log_outcome.sql"),
    ),
    (
        "0015_session_trace",
        include_str!("../migrations/0015_session_trace.sql"),
    ),
    (
        "0016_code_freshness",
        include_str!("../migrations/0016_code_freshness.sql"),
    ),
    (
        "0017_code_vault",
        include_str!("../migrations/0017_code_vault.sql"),
    ),
    (
        "0018_code_vault_visibility",
        include_str!("../migrations/0018_code_vault_visibility.sql"),
    ),
    (
        "0019_read_usage_counters",
        include_str!("../migrations/0019_read_usage_counters.sql"),
    ),
    (
        "0020_ann_sqlite_vec",
        include_str!("../migrations/0020_ann_sqlite_vec.sql"),
    ),
    (
        "0021_project_map_section_backfill",
        include_str!("../migrations/0021_project_map_section_backfill.sql"),
    ),
    (
        "0022_proactive_surface",
        include_str!("../migrations/0022_proactive_surface.sql"),
    ),
    (
        "0023_proactive_recall_sessions",
        include_str!("../migrations/0023_proactive_recall_sessions.sql"),
    ),
    (
        "0024_identity_section_backfill",
        include_str!("../migrations/0024_identity_section_backfill.sql"),
    ),
    (
        "0025_identity_title_backfill",
        include_str!("../migrations/0025_identity_title_backfill.sql"),
    ),
    (
        "0026_scheduled_task_health",
        include_str!("../migrations/0026_scheduled_task_health.sql"),
    ),
    (
        "0027_metric_sample",
        include_str!("../migrations/0027_metric_sample.sql"),
    ),
];

/// Migrations dont l'application nécessite une extension SQLite chargée.
///
/// Ces migrations sont ignorées silencieusement si l'extension n'est pas disponible
/// (détectée via `SELECT vec_version()`). La migration sera rejouée au prochain
/// démarrage si l'extension est chargée.
///
/// Format : préfixe du nom de version suffisant pour l'identification.
const EXTENSION_REQUIRED: &[(&str, &str)] = &[
    // 0020 : nécessite sqlite-vec vec0 (sqlite3_auto_extension avant open()).
    ("0020_ann_sqlite_vec", "vec_version"),
];

/// Vérifie si une migration nécessite une extension et si celle-ci est disponible.
///
/// Retourne `true` si la migration peut être appliquée :
/// - Soit elle ne nécessite pas d'extension.
/// - Soit l'extension requise est disponible (test SQL réussi).
fn extension_available_for(conn: &Connection, version: &str) -> bool {
    for (prefix, probe_fn) in EXTENSION_REQUIRED {
        if version.starts_with(prefix) {
            // Tester la disponibilité de l'extension via une requête probe.
            let available = conn
                .query_row(&format!("SELECT {probe_fn}()"), [], |_| Ok(()))
                .is_ok();
            return available;
        }
    }
    true // Pas de contrainte d'extension.
}

/// Applies migrations not yet recorded in `_schema_migrations`.
///
/// Bootstrap: creates `_schema_migrations` if the table does not yet exist.
/// Idempotent: safe to call multiple times with no side effects.
///
/// ## Migrations conditionnelles
///
/// Certaines migrations (ex. 0020_ann_sqlite_vec) nécessitent qu'une extension SQLite
/// soit chargée. Si l'extension est absente, la migration est ignorée avec un log warning.
/// Elle sera appliquée au prochain appel de `run()` si l'extension est alors disponible.
pub async fn run(conn: &Arc<Mutex<Connection>>) -> Result<(), GradatumError> {
    let conn = conn.lock().await;

    // Bootstrap : crée la table de tracking si absente.
    // IMPORTANT : le script 0001_phase1.sql crée aussi _schema_migrations et insère
    // la ligne — mais uniquement pour son propre run. Pour les migrations futures, ce
    // bootstrap garantit que la table existe quelle que soit l'état de la DB.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _schema_migrations (
            version TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )
    .map_err(|e| GradatumError::Storage(format!("bootstrap _schema_migrations : {e}")))?;

    for (version, sql) in MIGRATIONS {
        let already_applied: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = ?1)",
                [version],
                |row| row.get(0),
            )
            .map_err(|e| {
                GradatumError::Storage(format!("vérification migration {version} : {e}"))
            })?;

        if already_applied {
            continue;
        }

        // Migration conditionnelle : vérifier si l'extension requise est disponible.
        if !extension_available_for(&conn, version) {
            tracing::warn!(
                version = %version,
                "migration ignorée : extension SQLite non chargée — \
                 sera appliquée au prochain démarrage avec l'extension"
            );
            continue;
        }

        // Le batch SQL inclut l'INSERT dans _schema_migrations en fin de fichier.
        conn.execute_batch(sql).map_err(|e| {
            GradatumError::Storage(format!("application migration {version} : {e}"))
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vérifie que chaque fichier *.sql présent dans migrations/ est enregistré dans
    /// le const array MIGRATIONS.
    ///
    /// Un fichier non câblé est une migration silencieusement morte (jamais appliquée
    /// sur les nouvelles installations ni les DB existantes). Ce test force la détection
    /// à la compilation du test plutôt qu'en production.
    ///
    /// Le test liste le répertoire à l'exécution (pas compile-time) car `include_dir!`
    /// n'est pas une dépendance du crate. Le périmètre est suffisant : le CI l'attrape
    /// avant tout merge.
    #[test]
    fn all_sql_files_are_registered_in_migrations_array() {
        // Répertoire des migrations — chemin relatif depuis la racine du crate,
        // résolu via CARGO_MANIFEST_DIR injecté au moment de la compilation des tests.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let migrations_dir = std::path::Path::new(manifest_dir).join("migrations");

        let mut sql_files: Vec<String> = std::fs::read_dir(&migrations_dir)
            .unwrap_or_else(|e| panic!("impossible de lire {}: {}", migrations_dir.display(), e))
            .filter_map(|entry| entry.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                if name.ends_with(".sql") {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        sql_files.sort();

        let registered_versions: Vec<&str> = MIGRATIONS.iter().map(|(v, _)| *v).collect();

        for sql_file in &sql_files {
            // Nom de version = nom de fichier sans extension .sql.
            let version = sql_file.trim_end_matches(".sql");
            assert!(
                registered_versions.contains(&version),
                "fichier migrations/{sql_file} non enregistré dans MIGRATIONS — \
                 ajouter l'entrée (\"{version}\", include_str!(\"../migrations/{sql_file}\")) \
                 après la dernière entrée existante"
            );
        }

        // Symétrie : pas d'entrée fantôme dans MIGRATIONS sans fichier .sql correspondant.
        for version in &registered_versions {
            let sql_file = format!("{version}.sql");
            assert!(
                sql_files.contains(&sql_file),
                "MIGRATIONS contient \"{version}\" mais migrations/{sql_file} est absent"
            );
        }
    }

    /// Vérifie que la migration 0011 backfille correctement les notes council.
    ///
    /// Scénario :
    /// - note A : section='reference', body contient '[COUNCIL]' → doit passer à 'council'
    /// - note B : section='reference', body sans marqueur     → reste 'reference'
    /// - note C : section='council'   déjà correct            → reste 'council', c_kind aligné
    #[tokio::test]
    async fn migration_0011_backfills_council_section() {
        let conn = rusqlite::Connection::open_in_memory()
            .expect("connexion in-memory — ne peut pas échouer");
        let conn = Arc::new(tokio::sync::Mutex::new(conn));

        // Appliquer toutes les migrations jusqu'à 0010 inclus pour avoir le schéma complet.
        // On réutilise run() qui arrêtera après 0010 si 0011 n'était pas câblé,
        // mais ici 0011 est bien dans MIGRATIONS — run() l'appliquera.
        // On insère les notes de test AVANT run() pour que 0011 les backfille.
        {
            let locked = conn.lock().await;

            // Bootstrapper uniquement les migrations 0001 à 0010 manuellement afin
            // d'insérer les notes test AVANT que 0011 ne tourne.
            // Stratégie : exécuter run() sur un sous-ensemble est impossible sans
            // refactoring. On utilise une connexion séparée pour préparer les données
            // avant d'appliquer la dernière migration.
            //
            // Approche retenue : appliquer toutes les migrations via run() d'abord,
            // puis insérer les notes avec les anciennes valeurs (simulate pre-0011 state),
            // puis ré-appliquer run() (idempotent pour 0001-0011) — 0011 ne se re-lance
            // pas car _schema_migrations le bloque.
            // On insère donc APRÈS run() avec section='reference' + marqueur et on vérifie
            // via un UPDATE manuel reproduisant la logique 0011.
            //
            // Méthode correcte : insérer avant le premier run(), en bootstrappant
            // _schema_migrations manuellement pour qu'elle liste 0001-0010 mais pas 0011.
            locked
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS _schema_migrations (
                        version TEXT PRIMARY KEY,
                        applied_at INTEGER NOT NULL
                    );",
                )
                .expect("bootstrap _schema_migrations");
        }

        // Insérer les versions 0001-0010 comme déjà appliquées pour que run() ne les joue pas.
        // Puis insérer des notes de test AVANT que 0011 soit appliqué.
        {
            let locked = conn.lock().await;

            // Appliquer les migrations 0001-0010 directement (schéma réel).
            for (version, sql) in MIGRATIONS.iter().take(10) {
                let already: bool = locked
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = ?1)",
                        [version],
                        |r| r.get(0),
                    )
                    .expect("query EXISTS");
                if !already {
                    locked
                        .execute_batch(sql)
                        .unwrap_or_else(|e| panic!("migration {version} : {e}"));
                }
            }

            // Insérer 3 notes de test représentant l'état pré-0011.
            // Colonnes NOT NULL (schéma 0001) : id, vault_id, section, status,
            // schema_version, created, content_hash, body_text.
            // c_kind/doc_kind existent (migration 0008, déjà appliquée).
            // content_hash : 32 zéros en BLOB hex suffisent pour le test.
            locked
                .execute_batch(
                    "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, c_kind, doc_kind)
                     VALUES
                       ('01AAAA', 'test-vault', 'reference', 'live', 1, 0, X'0000000000000000000000000000000000000000000000000000000000000000', '[COUNCIL] verdict go 4/4', 'semantic', 'Reference'),
                       ('01BBBB', 'test-vault', 'reference', 'live', 1, 0, X'0000000000000000000000000000000000000000000000000000000000000001', 'pattern architecture',      'semantic', 'Reference'),
                       ('01CCCC', 'test-vault', 'council',   'live', 1, 0, X'0000000000000000000000000000000000000000000000000000000000000002', 'déjà en section council',   'semantic', 'Reference');",
                )
                .expect("insert notes de test");
        }

        // Maintenant appliquer run() complet — seule 0011 sera exécutée (0001-0010 déjà marquées).
        run(&conn).await.expect("run() avec 0011");

        // Vérifications.
        let locked = conn.lock().await;

        // Note A : section doit être 'council', c_kind='episodic', doc_kind='Event'.
        let (section_a, c_kind_a, doc_kind_a): (String, String, String) = locked
            .query_row(
                "SELECT section, c_kind, doc_kind FROM notes WHERE id='01AAAA'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("note A");
        assert_eq!(
            section_a, "council",
            "note A : section attendue 'council', obtenu '{section_a}'"
        );
        assert_eq!(
            c_kind_a, "episodic",
            "note A : c_kind attendu 'episodic', obtenu '{c_kind_a}'"
        );
        assert_eq!(
            doc_kind_a, "Event",
            "note A : doc_kind attendu 'Event', obtenu '{doc_kind_a}'"
        );

        // Note B : section reste 'reference' (pas de marqueur [COUNCIL]).
        let section_b: String = locked
            .query_row("SELECT section FROM notes WHERE id='01BBBB'", [], |r| {
                r.get(0)
            })
            .expect("note B");
        assert_eq!(
            section_b, "reference",
            "note B : doit rester 'reference', obtenu '{section_b}'"
        );

        // Note C : déjà 'council' — c_kind + doc_kind corrigés par l'étape 2.
        let (section_c, c_kind_c, doc_kind_c): (String, String, String) = locked
            .query_row(
                "SELECT section, c_kind, doc_kind FROM notes WHERE id='01CCCC'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("note C");
        assert_eq!(
            section_c, "council",
            "note C : section doit rester 'council', obtenu '{section_c}'"
        );
        assert_eq!(
            c_kind_c, "episodic",
            "note C : c_kind attendu 'episodic', obtenu '{c_kind_c}'"
        );
        assert_eq!(
            doc_kind_c, "Event",
            "note C : doc_kind attendu 'Event', obtenu '{doc_kind_c}'"
        );

        // Vérifier que 0011 est bien enregistrée dans _schema_migrations.
        let applied: bool = locked
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0011_council_section_backfill')",
                [],
                |r| r.get(0),
            )
            .expect("check _schema_migrations");
        assert!(
            applied,
            "0011_council_section_backfill absent de _schema_migrations après run()"
        );
    }

    /// Vérifie que la migration 0010 ajoute les colonnes provenance/trust sur notes
    /// et crée la table redirect_table.
    /// Appelé deux fois sur la même connexion pour valider l'idempotence du runner
    /// (le guard _schema_migrations empêche la ré-application des ALTER TABLE).
    #[tokio::test]
    async fn migration_0010_adds_provenance_trust_and_redirect_table() {
        // Connexion in-memory brute — pas de SqliteIndex pour garder le test
        // dans migrations.rs sans dépendance circulaire sur sqlite.rs.
        let conn = rusqlite::Connection::open_in_memory()
            .expect("connexion in-memory — ne peut pas échouer");
        let conn = Arc::new(tokio::sync::Mutex::new(conn));

        // Première application : toutes les migrations jusqu'à 0010.
        run(&conn).await.expect("run() première fois");

        // Deuxième application : idempotence — le runner doit passer sans erreur.
        run(&conn).await.expect("run() deuxième fois (idempotence)");

        let locked = conn.lock().await;

        // Vérifier colonnes provenance et trust sur la table notes.
        let cols: Vec<String> = locked
            .prepare("SELECT name FROM pragma_table_info('notes')")
            .expect("pragma_table_info — doit compiler")
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query_map — doit compiler")
            .filter_map(|r| r.ok())
            .collect();

        assert!(
            cols.contains(&"provenance".to_string()),
            "colonne provenance absente — migration 0010 non appliquée. cols={cols:?}"
        );
        assert!(
            cols.contains(&"trust".to_string()),
            "colonne trust absente — migration 0010 non appliquée. cols={cols:?}"
        );

        // Vérifier existence de la table redirect_table.
        let n: i64 = locked
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='redirect_table'",
                [],
                |r| r.get(0),
            )
            .expect("sqlite_master — doit retourner un entier");
        assert_eq!(n, 1, "table redirect_table absente après migration 0010");
    }

    // ── Tests migration 0013 — temporal_index (F-55) ─────────────────────────

    /// Vérifie que la migration 0013 backfille correctement les notes pré-existantes.
    ///
    /// Scénario :
    /// - note 1 : section='decisions', doc_kind='Static', created=1_000_000 → anchor_ms=1_000_000, anchor_src='created'
    /// - note 2 : section='debug',     doc_kind='Event',  created=2_000_000 → anchor_ms=2_000_000, doc_kind='Event'
    /// - note 3 : section='reference', doc_kind=NULL,     created=3_000_000 → doc_kind='Static' (COALESCE)
    ///
    /// Méthode : appliquer 0001-0012 manuellement, insérer les notes, appliquer run() (0013 seule).
    #[tokio::test]
    async fn migration_0013_backfills_all_existing_notes() {
        let conn_raw = rusqlite::Connection::open_in_memory()
            .expect("connexion in-memory — ne peut pas échouer");
        let conn = Arc::new(tokio::sync::Mutex::new(conn_raw));

        // Bootstrapper _schema_migrations.
        {
            let locked = conn.lock().await;
            locked
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS _schema_migrations (
                        version TEXT PRIMARY KEY,
                        applied_at INTEGER NOT NULL
                    );",
                )
                .expect("bootstrap _schema_migrations");
        }

        // Appliquer les 12 premières migrations (0001-0012) pour avoir schéma complet.
        {
            let locked = conn.lock().await;
            for (version, sql) in MIGRATIONS.iter().take(12) {
                let already: bool = locked
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = ?1)",
                        [version],
                        |r| r.get(0),
                    )
                    .expect("check migration");
                if !already {
                    locked
                        .execute_batch(sql)
                        .unwrap_or_else(|e| panic!("migration {version} : {e}"));
                }
            }

            // Insérer 3 notes de test représentant l'état pré-0013.
            // Colonnes NOT NULL + doc_kind (migration 0008).
            locked
                .execute_batch(
                    "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, doc_kind)
                     VALUES
                       ('01TTEST1', 'main', 'decisions', 'live', 1, 1000000, X'0000000000000000000000000000000000000000000000000000000000000001', 'note 1', 'Static'),
                       ('01TTEST2', 'main', 'debug',     'live', 1, 2000000, X'0000000000000000000000000000000000000000000000000000000000000002', 'note 2', 'Event'),
                       ('01TTEST3', 'main', 'reference', 'live', 1, 3000000, X'0000000000000000000000000000000000000000000000000000000000000003', 'note 3', NULL);",
                )
                .expect("insert notes pré-0013");
        }

        // Appliquer run() complet — seule 0013 sera exécutée (0001-0012 déjà marquées).
        run(&conn).await.expect("run() avec 0013");

        let locked = conn.lock().await;

        // Les 3 notes doivent avoir une entrée temporal_index.
        let count: i64 = locked
            .query_row(
                "SELECT COUNT(*) FROM temporal_index WHERE vault_id='main'",
                [],
                |r| r.get(0),
            )
            .expect("count temporal_index");
        assert_eq!(
            count, 3,
            "backfill doit couvrir 100% des notes — attendu 3, obtenu {count}"
        );

        // Toutes entrées backfillées doivent avoir anchor_src='created'.
        let non_created: i64 = locked
            .query_row(
                "SELECT COUNT(*) FROM temporal_index WHERE anchor_src != 'created'",
                [],
                |r| r.get(0),
            )
            .expect("count non-created");
        assert_eq!(
            non_created, 0,
            "toutes entrées backfillées doivent avoir anchor_src='created', {non_created} différentes"
        );

        // Note 1 : anchor_ms = created = 1_000_000.
        let (anchor1, src1, doc1): (i64, String, String) = locked
            .query_row(
                "SELECT anchor_ms, anchor_src, doc_kind FROM temporal_index WHERE note_id='01TTEST1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("entrée 01TTEST1");
        assert_eq!(
            anchor1, 1_000_000,
            "anchor_ms note 1 : attendu 1_000_000, obtenu {anchor1}"
        );
        assert_eq!(
            src1, "created",
            "anchor_src note 1 : attendu 'created', obtenu '{src1}'"
        );
        assert_eq!(
            doc1, "Static",
            "doc_kind note 1 : attendu 'Static', obtenu '{doc1}'"
        );

        // Note 2 : doc_kind='Event' (section debug).
        let doc2: String = locked
            .query_row(
                "SELECT doc_kind FROM temporal_index WHERE note_id='01TTEST2'",
                [],
                |r| r.get(0),
            )
            .expect("doc_kind 01TTEST2");
        assert_eq!(
            doc2, "Event",
            "doc_kind note 2 (debug) : attendu 'Event', obtenu '{doc2}'"
        );

        // Note 3 : doc_kind=NULL → COALESCE → 'Static'.
        let doc3: String = locked
            .query_row(
                "SELECT doc_kind FROM temporal_index WHERE note_id='01TTEST3'",
                [],
                |r| r.get(0),
            )
            .expect("doc_kind 01TTEST3");
        assert_eq!(
            doc3, "Static",
            "doc_kind note 3 (NULL) : attendu 'Static' via COALESCE, obtenu '{doc3}'"
        );

        // 0013 est bien enregistrée dans _schema_migrations.
        let applied: bool = locked
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0013_temporal_index')",
                [],
                |r| r.get(0),
            )
            .expect("check _schema_migrations");
        assert!(
            applied,
            "0013_temporal_index absent de _schema_migrations après run()"
        );
    }

    // ── Tests migration 0014 — event_log.outcome (F-19 M6) ──────────────────

    /// Vérifie que la migration 0014 ajoute la colonne `outcome` (nullable) sur
    /// event_log et crée l'index associé, via run() complet.
    ///
    /// Leçon migration v0.4.x : toute migration doit avoir un test qui prouve
    /// empiriquement son application (pas seulement son enregistrement).
    /// Le double run() vérifie aussi l'idempotence (le guard _schema_migrations
    /// empêche la ré-application de l'ALTER TABLE).
    #[tokio::test]
    async fn migration_0014_adds_outcome_column_and_index() {
        let conn = rusqlite::Connection::open_in_memory()
            .expect("connexion in-memory — ne peut pas échouer");
        let conn = Arc::new(tokio::sync::Mutex::new(conn));

        // Première application : toutes les migrations jusqu'à 0014 incluse.
        run(&conn).await.expect("run() première fois");
        // Deuxième application : idempotence — le runner passe sans erreur.
        run(&conn).await.expect("run() deuxième fois (idempotence)");

        let locked = conn.lock().await;

        // La colonne outcome doit exister sur event_log.
        let cols: Vec<String> = locked
            .prepare("SELECT name FROM pragma_table_info('event_log')")
            .expect("pragma_table_info — doit compiler")
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query_map — doit compiler")
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            cols.contains(&"outcome".to_string()),
            "colonne outcome absente — migration 0014 non appliquée. cols={cols:?}"
        );

        // L'index idx_event_log_outcome doit exister.
        let n_index: i64 = locked
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_event_log_outcome'",
                [],
                |r| r.get(0),
            )
            .expect("sqlite_master index — doit retourner un entier");
        assert_eq!(n_index, 1, "index idx_event_log_outcome absent après 0014");

        // La colonne est nullable : un INSERT sans outcome doit réussir avec NULL.
        // (schéma event_log = 0006 ; on insère le minimum requis NOT NULL).
        locked
            .execute(
                "INSERT INTO event_log (ts, tenant_id, route, model_alias, provider, status_code, latency_ms, created_at)
                 VALUES (0, 'main', '/v1/embeddings', 'bge-m3', 'engine-embed', 200, 10, 0)",
                [],
            )
            .expect("insert sans outcome doit réussir (colonne nullable)");
        let outcome: Option<String> = locked
            .query_row("SELECT outcome FROM event_log LIMIT 1", [], |r| r.get(0))
            .expect("select outcome");
        assert!(
            outcome.is_none(),
            "outcome doit être NULL par défaut (legacy/best-effort), obtenu {outcome:?}"
        );

        // 0014 enregistrée dans _schema_migrations.
        let applied: bool = locked
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0014_event_log_outcome')",
                [],
                |r| r.get(0),
            )
            .expect("check _schema_migrations");
        assert!(
            applied,
            "0014_event_log_outcome absent de _schema_migrations après run()"
        );
    }

    /// Vérifie l'idempotence du backfill : un double backfill (INSERT OR IGNORE) ne
    /// crée pas de doublons ni n'écrase les entrées existantes.
    #[tokio::test]
    async fn migration_0013_backfill_is_idempotent() {
        use crate::SqliteIndex;

        // Une DB fraîche : 0013 déjà appliquée (toutes migrations via open_in_memory).
        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

        // Insérer une note après migration (pour s'assurer que backfill_temporal_index
        // peut gérer des notes non encore backfillées).
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, doc_kind) \
                 VALUES ('01BIDEM1', 'main', 'decisions', 'live', 1, 9_000_000, \
                         X'FF00000000000000000000000000000000000000000000000000000000000001', \
                         'body backfill idem', 'Static')",
                [],
            )
            .expect("insert note backfill idem");
        }

        // Premier backfill — insère l'entrée manquante.
        let inserted1 = idx.backfill_temporal_index().await.expect("backfill 1");
        assert_eq!(
            inserted1, 1,
            "premier backfill doit insérer 1 note, obtenu {inserted1}"
        );

        // Second backfill — INSERT OR IGNORE → 0 insertions.
        let inserted2 = idx.backfill_temporal_index().await.expect("backfill 2");
        assert_eq!(
            inserted2, 0,
            "second backfill sur DB inchangée doit retourner 0, obtenu {inserted2}"
        );

        // L'entrée n'a pas été dupliquée.
        let conn = idx.conn.lock().await;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM temporal_index WHERE note_id='01BIDEM1'",
                [],
                |r| r.get(0),
            )
            .expect("count idem");
        assert_eq!(
            count, 1,
            "entrée dupliquée après double backfill — count={count}"
        );
    }
}
