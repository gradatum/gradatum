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
    (
        "0028_archive_index",
        include_str!("../migrations/0028_archive_index.sql"),
    ),
    (
        "0029_note_usage",
        include_str!("../migrations/0029_note_usage.sql"),
    ),
    (
        "0030_tenants_grants",
        include_str!("../migrations/0030_tenants_grants.sql"),
    ),
    (
        "0031_tenants_status_deleted",
        include_str!("../migrations/0031_tenants_status_deleted.sql"),
    ),
    (
        "0032_notes_composite_pk",
        include_str!("../migrations/0032_notes_composite_pk.sql"),
    ),
    (
        "0033_child_tables_vault_id",
        include_str!("../migrations/0033_child_tables_vault_id.sql"),
    ),
    (
        "0034_child_tables_composite_pk",
        include_str!("../migrations/0034_child_tables_composite_pk.sql"),
    ),
    (
        "0035_redirect_table_vault_id",
        include_str!("../migrations/0035_redirect_table_vault_id.sql"),
    ),
    (
        "0036_override_locus_bearer_vault",
        include_str!("../migrations/0036_override_locus_bearer_vault.sql"),
    ),
    (
        "0037_archive_active_vault_scope",
        include_str!("../migrations/0037_archive_active_vault_scope.sql"),
    ),
    (
        "0038_ann_composite_vault",
        include_str!("../migrations/0038_ann_composite_vault.sql"),
    ),
    (
        "0039_child_tables_composite_fk",
        include_str!("../migrations/0039_child_tables_composite_fk.sql"),
    ),
    (
        "0040_grants_section_scope",
        include_str!("../migrations/0040_grants_section_scope.sql"),
    ),
    (
        "0041_feature_counter",
        include_str!("../migrations/0041_feature_counter.sql"),
    ),
    (
        "0042_agent_vault_grants",
        include_str!("../migrations/0042_agent_vault_grants.sql"),
    ),
    (
        "0043_project_map_roles",
        include_str!("../migrations/0043_project_map_roles.sql"),
    ),
];

/// Migrations that require a loaded SQLite extension to be applied.
///
/// These migrations are silently skipped when the extension is unavailable — probed with
/// `SELECT vec_version()` — and are retried on the next startup once it is loaded.
///
/// Each entry holds a version-name prefix long enough to identify the migration.
const EXTENSION_REQUIRED: &[(&str, &str)] = &[
    // 0020 : nécessite sqlite-vec vec0 (sqlite3_auto_extension avant open()).
    ("0020_ann_sqlite_vec", "vec_version"),
    // 0038 (A4) : DROP + CREATE VIRTUAL TABLE ... USING vec0 — même contrainte que 0020.
    // Sans l'extension chargée (ANN OFF = ann_backend BruteForce), la migration est ignorée
    // → byte-identical au deploy (cf. en-tête 0038_ann_composite_vault.sql).
    ("0038_ann_composite_vault", "vec_version"),
];

/// Checks whether a migration needs an extension and, if so, whether it is available.
///
/// Returns `true` when the migration can be applied — either it needs no extension, or
/// the required extension answered the probe query successfully.
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
/// ## Conditional migrations
///
/// Some migrations (for instance `0020_ann_sqlite_vec`) require a SQLite extension to be
/// loaded. When the extension is missing, the migration is skipped with a warning log and
/// applied on a later `run()` call, once the extension is available.
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
            .map_err(|e| GradatumError::Storage(format!("migration check {version}: {e}")))?;

        if already_applied {
            continue;
        }

        // Migration conditionnelle : vérifier si l'extension requise est disponible.
        if !extension_available_for(&conn, version) {
            tracing::warn!(
                version = %version,
                "migration skipped: SQLite extension not loaded — \
                 will be applied on the next startup with the extension"
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
                // Les scripts de rollback manuel `*.down.sql` ne sont pas des migrations
                // forward (le runner est forward-only) → exclus de l'enregistrement MIGRATIONS.
                if name.ends_with(".sql") && !name.ends_with(".down.sql") {
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

    /// Normalise un SQL en une séquence de tokens séparés par un espace unique, en
    /// **retirant d'abord les commentaires de ligne `--`** (on teste le DDL réel, pas la
    /// documentation d'en-tête — laquelle cite p. ex. le schéma 0020 `note_id TEXT PRIMARY
    /// KEY`, à ne pas confondre avec la déclaration effective). Robuste à l'indentation.
    fn normalize_sql(sql: &str) -> String {
        sql.lines()
            // Coupe chaque ligne au premier `--` (commentaires pleine-ligne dans ces fichiers).
            .map(|line| line.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// T5 (a) — la migration 0038 (A4) est câblée dans `MIGRATIONS`, gatée `vec_version`
    /// dans `EXTENSION_REQUIRED`, et déclare le schéma vec0 **PARTITION KEY natif**
    /// (`vault_id`/`embedder_id` PARTITION KEY, `note_id` colonne ordinaire — PLUS de PK
    /// globale sur `note_id`).
    ///
    /// C'est ce retrait de la PK globale qui autorise la coexistence d'un même ULID sur
    /// deux partitions `(vault_id, embedder_id)` → fin de l'éviction cross-vault (A4).
    #[test]
    fn migration_0038_registered() {
        // Câblée dans MIGRATIONS (sinon `all_sql_files_are_registered_in_migrations_array`
        // échouerait déjà — ici on cible explicitement 0038 + le contenu du schéma).
        let entry = MIGRATIONS
            .iter()
            .find(|(v, _)| *v == "0038_ann_composite_vault")
            .expect("0038_ann_composite_vault doit être câblée dans MIGRATIONS");
        let up = normalize_sql(entry.1);

        // Gatée sur l'extension vec0 (comme 0020) → ignorée à ANN OFF (byte-identical deploy).
        assert!(
            EXTENSION_REQUIRED
                .iter()
                .any(|(v, probe)| *v == "0038_ann_composite_vault" && *probe == "vec_version"),
            "0038 doit être gatée `vec_version` dans EXTENSION_REQUIRED"
        );

        // Schéma vec0 PARTITION KEY natif.
        assert!(
            up.contains("USING vec0"),
            "0038 doit créer une virtual table vec0"
        );
        assert!(
            up.contains("vault_id TEXT PARTITION KEY"),
            "vault_id doit rester PARTITION KEY"
        );
        assert!(
            up.contains("embedder_id TEXT PARTITION KEY"),
            "embedder_id doit rester PARTITION KEY"
        );
        assert!(
            up.contains("note_id TEXT,"),
            "note_id doit être une colonne ordinaire (pas de PK globale)"
        );
        // Cœur du fix A4 : `note_id` ne doit PLUS être PRIMARY KEY (clé globale = éviction).
        assert!(
            !up.contains("note_id TEXT PRIMARY KEY"),
            "0038 doit RETIRER la PK globale `note_id` (sinon l'éviction cross-vault persiste)"
        );
    }

    /// T5 (c) — le rollback manuel `.down` restaure exactement le schéma 0020
    /// (`note_id TEXT PRIMARY KEY` global) et retire la ligne de registre.
    #[test]
    fn migration_0038_down_shape() {
        let down = normalize_sql(include_str!(
            "../migrations/0038_ann_composite_vault.down.sql"
        ));
        assert!(
            down.contains("DROP TABLE IF EXISTS note_embeddings_ann"),
            ".down doit dropper la table avant recréation"
        );
        assert!(
            down.contains("note_id TEXT PRIMARY KEY"),
            ".down doit restaurer la PK globale `note_id` (schéma 0020)"
        );
        assert!(
            down.contains("USING vec0"),
            ".down doit recréer une virtual table vec0"
        );
        assert!(
            down.contains(
                "DELETE FROM _schema_migrations WHERE version = '0038_ann_composite_vault'"
            ),
            ".down doit retirer la ligne de registre pour permettre le ré-jeu de 0038"
        );
    }

    /// T5 (b) — modèle composite prouvé sur une **table shadow** (convention C4-1e :
    /// vec0 est bin-only, absent des tests `gradatum-index` — la fidélité vec0 réelle
    /// reste couverte par les tests `#[ignore = "requiert libvec0"]`).
    ///
    /// La table shadow miroir déclare l'identité `(vault_id, embedder_id, note_id)` — image
    /// plate du PARTITION KEY vec0 `(vault_id, embedder_id)` + colonne ordinaire `note_id` :
    /// un même ULID coexiste sur deux vaults (PLUS de PK globale sur `note_id`) ET sur deux
    /// embedders d'un même vault (`embedder_id` est une clé de partition à part entière),
    /// tandis que le TRIPLET reste unique (l'upsert scopé y garantit une ligne).
    #[test]
    fn ann_shadow_composite_unique() {
        let conn = rusqlite::Connection::open_in_memory()
            .expect("connexion in-memory — ne peut pas échouer");
        conn.execute_batch(
            "CREATE TABLE note_embeddings_ann (
                 note_id     TEXT NOT NULL,
                 vault_id    TEXT NOT NULL,
                 embedder_id TEXT NOT NULL,
                 vector      BLOB,
                 PRIMARY KEY (vault_id, embedder_id, note_id)
             );",
        )
        .expect("create shadow ann table");

        let ulid = "01ANNSHADOWCOMPOSITE01";

        // Même ULID, deux vaults → LES DEUX doivent coexister (fin de l'éviction).
        conn.execute(
            "INSERT INTO note_embeddings_ann (note_id, vault_id, embedder_id, vector)
             VALUES (?1, 'main', 'bge-m3', X'00')",
            rusqlite::params![ulid],
        )
        .expect("insert vault main");
        conn.execute(
            "INSERT INTO note_embeddings_ann (note_id, vault_id, embedder_id, vector)
             VALUES (?1, 'vault-b', 'bge-m3', X'00')",
            rusqlite::params![ulid],
        )
        .expect("insert vault-b (même ULID) — doit coexister, PLUS de PK globale");

        let coexist: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM note_embeddings_ann WHERE note_id = ?1",
                rusqlite::params![ulid],
                |r| r.get(0),
            )
            .expect("count coexist");
        assert_eq!(
            coexist, 2,
            "le même ULID doit coexister sur `main` et `vault-b` (2 lignes) — pas d'éviction"
        );

        // Même ULID, même vault, DEUX embedders → LES DEUX doivent coexister : `embedder_id`
        // est une clé de partition, pas un attribut. Une PK `(vault_id, note_id)` rejetterait
        // cet INSERT et masquerait l'écrasement inter-embedder du chemin d'écriture.
        conn.execute(
            "INSERT INTO note_embeddings_ann (note_id, vault_id, embedder_id, vector)
             VALUES (?1, 'main', 'embed', X'00')",
            rusqlite::params![ulid],
        )
        .expect("insert 2e embedder du même vault — doit coexister (partition distincte)");

        let embedders_main: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM note_embeddings_ann WHERE note_id = ?1 AND vault_id = 'main'",
                rusqlite::params![ulid],
                |r| r.get(0),
            )
            .expect("count embedders main");
        assert_eq!(
            embedders_main, 2,
            "le même ULID doit porter une ligne par embedder dans `main` (2 partitions)"
        );

        // Le TRIPLET `(vault_id, embedder_id, note_id)` reste unique : un doublon exact
        // dans la même partition est rejeté.
        let dup = conn.execute(
            "INSERT INTO note_embeddings_ann (note_id, vault_id, embedder_id, vector)
             VALUES (?1, 'main', 'bge-m3', X'00')",
            rusqlite::params![ulid],
        );
        assert!(
            dup.is_err(),
            "(vault_id, embedder_id, note_id) doit rester unique (doublon exact rejeté)"
        );
    }

    /// Vérifie la migration 0040 (L3, F-121) : colonne `section` ajoutée, NULLABLE, et
    /// lignes héritées laissées à `NULL` (= grant vault-entier, aucune migration de données).
    #[tokio::test]
    async fn migration_0040_adds_nullable_section_without_touching_existing_rows() {
        let conn = rusqlite::Connection::open_in_memory()
            .expect("connexion in-memory — ne peut pas échouer");
        let conn = Arc::new(tokio::sync::Mutex::new(conn));
        run(&conn).await.expect("run des migrations");
        let c = conn.lock().await;

        // 1. La colonne existe et est nullable (`notnull = 0`), sans DEFAULT.
        let (notnull, dflt): (i64, Option<String>) = c
            .query_row(
                "SELECT \"notnull\", dflt_value FROM pragma_table_info('tenant_vault_grants')
                 WHERE name = 'section'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("colonne section absente de tenant_vault_grants");
        assert_eq!(notnull, 0, "`section` doit être NULLABLE");
        assert_eq!(dflt, None, "`section` ne doit pas porter de DEFAULT");

        // 2. Le seed 0030 (`main`↔`main`) reste vault-entier : aucune donnée migrée.
        let scoped: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM tenant_vault_grants WHERE section IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .expect("count grants scopés");
        assert_eq!(
            scoped, 0,
            "aucune ligne existante ne doit être bornée par 0040"
        );

        // 3. Une ligne bornée est acceptée (aucun CHECK ne l'interdit).
        c.execute(
            "INSERT INTO tenant_vault_grants (tenant_id, vault_id, access, section)
             VALUES ('main', 'other', 'read', 'lessons-learned')",
            [],
        )
        .expect("insertion d'un grant section-scopé doit être acceptée");
    }

    /// Vérifie la migration 0030 (C1, F-63) : tables créées, seed `main`↔`main` write,
    /// et re-jeu idempotent (A5 : `INSERT OR IGNORE` — aucun doublon au re-run).
    #[tokio::test]
    async fn migration_0030_seed_idempotent_and_replayable() {
        let conn = rusqlite::Connection::open_in_memory()
            .expect("connexion in-memory — ne peut pas échouer");
        let conn = Arc::new(tokio::sync::Mutex::new(conn));

        run(&conn).await.expect("premier run des migrations");

        // Re-jouer 0030 : retirer sa ligne de registre puis relancer le runner —
        // le batch complet (CREATE + seed) est ré-exécuté sur un schéma déjà peuplé.
        {
            let c = conn.lock().await;
            c.execute(
                "DELETE FROM _schema_migrations WHERE version = '0030_tenants_grants'",
                [],
            )
            .expect("delete registre 0030");
        }
        run(&conn)
            .await
            .expect("re-run des migrations (0030 re-joué)");

        let c = conn.lock().await;
        let tenants: i64 = c
            .query_row("SELECT COUNT(*) FROM tenants WHERE id = 'main'", [], |r| {
                r.get(0)
            })
            .expect("count tenants main");
        assert_eq!(tenants, 1, "seed tenant main unique après re-jeu");

        let grants: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM tenant_vault_grants
                 WHERE tenant_id = 'main' AND vault_id = 'main' AND access = 'write'",
                [],
                |r| r.get(0),
            )
            .expect("count grant main↔main");
        assert_eq!(grants, 1, "seed grant main↔main write unique après re-jeu");

        // CHECK constraint : une valeur access hors énumération est refusée à l'INSERT.
        let bad = c.execute(
            "INSERT INTO tenant_vault_grants (tenant_id, vault_id, access)
             VALUES ('main', 'other', 'admin')",
            [],
        );
        assert!(
            bad.is_err(),
            "access hors {{read,write}} doit violer le CHECK"
        );
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

    // ── Test migration 0032 — PK composite (vault_id, id) (C4-1d) ─────────────

    /// Vérifie que 0032 : (a) préserve les données existantes de `main`, (b) autorise
    /// deux lignes pour le même ULID dans des vaults distincts (PK composite), (c) retire
    /// les FK enfants (insert avec note_id inexistant ne casse plus), (d) reconstruit le FTS.
    #[tokio::test]
    async fn migration_0032_composite_pk_preserves_data_and_allows_cross_vault_rows() {
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory");
        let conn = Arc::new(tokio::sync::Mutex::new(conn));

        // Appliquer 0001..0031 puis seeder une note `main` (+ FTS) AVANT 0032.
        {
            let c = conn.lock().await;
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS _schema_migrations (version TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
            )
            .expect("bootstrap");
            for (version, sql) in MIGRATIONS.iter().take(31) {
                let already: bool = c
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = ?1)",
                        [version],
                        |r| r.get(0),
                    )
                    .expect("check");
                // Skip conditionnel des migrations à extension (0020 vec0) — parité `run()`.
                if !already && extension_available_for(&c, version) {
                    c.execute_batch(sql)
                        .unwrap_or_else(|e| panic!("migration {version} : {e}"));
                }
            }
            c.execute_batch(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text, title)
                 VALUES ('01MAINX', 'main', 'reference', 'live', 1, 0, X'00', 'main sphinx body', 'Main');
                 INSERT INTO notes_fts (rowid, body_text) SELECT rowid, body_text FROM notes WHERE id='01MAINX';",
            )
            .expect("seed main note + fts");
        }

        // Appliquer 0032.
        run(&conn).await.expect("run 0032");

        let c = conn.lock().await;

        // (a) Données préservées.
        let body: String = c
            .query_row(
                "SELECT body_text FROM notes WHERE id='01MAINX' AND vault_id='main'",
                [],
                |r| r.get(0),
            )
            .expect("note main préservée");
        assert_eq!(
            body, "main sphinx body",
            "contenu main inchangé après recreate"
        );

        // (b) PK composite : deux lignes même ULID, vaults distincts.
        c.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
             VALUES ('01MAINX', 'research', 'reference', 'live', 1, 0, X'01', 'research gizmo body')",
            [],
        )
        .expect("insert cross-vault même ULID doit réussir (PK composite)");
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM notes WHERE id='01MAINX'", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(n, 2, "deux lignes (main, research) pour le même ULID");

        // (c) Le FK id-only d'origine a bien été retiré par 0032, MAIS `run()` applique
        // toute la chaîne, dont 0039 qui REPOSE un FK composite `(vault_id, note_id) →
        // notes(vault_id, id) ON DELETE CASCADE` sur `note_audit_trail` (foreign_keys=ON en
        // fin de 0039). L'état final rejette donc un enfant orphelin : c'est la preuve que le
        // FK composite est actif après la chaîne complète.
        let orphan = c.execute(
            "INSERT INTO note_audit_trail (note_id, vault_id, event_type, actor_kind, actor_id, occurred_at)
             VALUES ('01NOEXIST', 'main', 'x', 'system', 's', 0)",
            [],
        );
        assert!(
            orphan.is_err(),
            "insert audit orphelin doit être rejeté par le FK composite reposé en 0039"
        );

        // (d) FTS reconstruit : le contenu de main reste indexé.
        let fts_hits: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM notes_fts WHERE notes_fts MATCH 'sphinx'",
                [],
                |r| r.get(0),
            )
            .expect("fts match");
        assert!(
            fts_hits >= 1,
            "rebuild FTS : 'sphinx' de main toujours indexé"
        );

        // 0032 enregistrée.
        let applied: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0032_notes_composite_pk')",
                [],
                |r| r.get(0),
            )
            .expect("check 0032");
        assert!(applied, "0032 enregistrée dans _schema_migrations");
    }

    // ── Tests migration 0033 — vault_id sur tables filles (C4-1e, Slice D2) ────

    /// Applique les migrations jusqu'à (non inclus) 0033 → schéma pré-0033 (0032).
    ///
    /// `take_while` sur le nom de version est robuste à l'insertion de migrations
    /// avant 0033 (contrairement à un `take(n)` positionnel) et ignore tout ce qui
    /// suit 0033.
    async fn apply_migrations_before_0033(conn: &Arc<Mutex<Connection>>) {
        let c = conn.lock().await;
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS _schema_migrations (version TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
        )
        .expect("bootstrap _schema_migrations");
        for (version, sql) in MIGRATIONS
            .iter()
            .take_while(|(v, _)| *v != "0033_child_tables_vault_id")
        {
            let already: bool = c
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = ?1)",
                    [version],
                    |r| r.get(0),
                )
                .expect("check migration");
            // Skip conditionnel des migrations à extension (0020 vec0) — parité `run()`.
            if !already && extension_available_for(&c, version) {
                c.execute_batch(sql)
                    .unwrap_or_else(|e| panic!("migration {version} : {e}"));
            }
        }
    }

    /// Applique explicitement le batch 0033 (et uniquement lui) sur une DB déjà à 0032.
    async fn apply_migration_0033(conn: &Arc<Mutex<Connection>>) {
        let sql = MIGRATIONS
            .iter()
            .find(|(v, _)| *v == "0033_child_tables_vault_id")
            .map(|(_, sql)| *sql)
            .expect("0033 enregistrée dans MIGRATIONS");
        let c = conn.lock().await;
        c.execute_batch(sql).expect("application 0033");
    }

    /// Empreinte structurelle d'une table : `(nom, type, notnull, pk)` par colonne.
    /// Immunisée au formatage du DDL (comparaison sémantique, pas textuelle).
    fn table_fingerprint(conn: &Connection, table: &str) -> Vec<(String, String, i64, i64)> {
        conn.prepare(&format!(
            "SELECT name, type, \"notnull\", pk FROM pragma_table_info('{table}')"
        ))
        .expect("prepare pragma_table_info")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .expect("query pragma_table_info")
        .map(|r| r.expect("row pragma_table_info"))
        .collect()
    }

    /// Empreinte des index d'une table (noms triés — inclut les auto-index de PK).
    fn index_fingerprint(conn: &Connection, table: &str) -> Vec<String> {
        let mut v: Vec<String> = conn
            .prepare(&format!("SELECT name FROM pragma_index_list('{table}')"))
            .expect("prepare pragma_index_list")
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query pragma_index_list")
            .map(|r| r.expect("row pragma_index_list"))
            .collect();
        v.sort();
        v
    }

    /// Seed une note `main` + une ligne fille pré-0033 (schéma sans `vault_id`) dans
    /// chacune des 3 tables ciblées, pour un `note_id` donné.
    async fn seed_note_and_children_pre_0033(conn: &Arc<Mutex<Connection>>, note_id: &str) {
        let c = conn.lock().await;
        c.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
             VALUES (?1, 'main', 'reference', 'live', 1, 0, X'00', 'body')",
            [note_id],
        )
        .expect("seed note main");
        c.execute(
            "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at)
             VALUES (?1, 'bge-m3', X'00000000', 1, NULL, 0)",
            [note_id],
        )
        .expect("seed embedding pré-0033");
        c.execute(
            "INSERT INTO note_history (note_id, from_version, to_version, diff_text, committed_at)
             VALUES (?1, 0, 1, 'diff', 0)",
            [note_id],
        )
        .expect("seed history pré-0033");
        c.execute(
            "INSERT INTO note_audit_trail (note_id, event_type, actor_kind, actor_id, occurred_at)
             VALUES (?1, 'created', 'system', 'test', 0)",
            [note_id],
        )
        .expect("seed audit pré-0033");
    }

    /// Backfill 0033 : chaque ligne fille reçoit le `vault_id` de sa note parente,
    /// 0 NULL, 0 divergence (Steps 1-2 du brief D2).
    #[tokio::test]
    async fn migration_0033_backfills_vault_id_no_null_no_divergence() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0033(&conn).await;
        seed_note_and_children_pre_0033(&conn, "01D2BACKFILL0001").await;
        apply_migration_0033(&conn).await;

        let c = conn.lock().await;
        for child in ["note_embeddings", "note_history", "note_audit_trail"] {
            // 0 NULL après backfill.
            let nulls: i64 = c
                .query_row(
                    &format!("SELECT COUNT(*) FROM {child} WHERE vault_id IS NULL"),
                    [],
                    |r| r.get(0),
                )
                .expect("count NULL");
            assert_eq!(
                nulls, 0,
                "{child} : {nulls} vault_id NULL après backfill 0033"
            );

            // 0 divergence vs la note parente.
            let diverge: i64 = c
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {child} c JOIN notes n ON c.note_id = n.id \
                         WHERE c.vault_id != n.vault_id"
                    ),
                    [],
                    |r| r.get(0),
                )
                .expect("count divergence");
            assert_eq!(
                diverge, 0,
                "{child} : {diverge} vault_id divergents de la note parente"
            );
        }
    }

    /// Réversibilité 0033 : up puis rollback (`.down.sql`) restaure le schéma pré-0033
    /// (colonnes/PK/index) ET les données (Step 3 du brief D2).
    #[tokio::test]
    async fn migration_0033_is_reversible() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0033(&conn).await;
        seed_note_and_children_pre_0033(&conn, "01D2REVERT00001").await;

        let tables = ["note_embeddings", "note_history", "note_audit_trail"];

        // Empreinte pré-0033 (schéma + données).
        let (schema_before, index_before, data_before) = {
            let c = conn.lock().await;
            let schema: Vec<_> = tables.iter().map(|t| table_fingerprint(&c, t)).collect();
            let indexes: Vec<_> = tables.iter().map(|t| index_fingerprint(&c, t)).collect();
            let emb: i64 = c
                .query_row(
                    "SELECT dim FROM note_embeddings WHERE note_id='01D2REVERT00001' AND embedder_id='bge-m3'",
                    [], |r| r.get(0),
                )
                .expect("emb pré");
            (schema, indexes, emb)
        };

        apply_migration_0033(&conn).await;

        // Rollback manuel via le script documenté `.down.sql`.
        {
            let c = conn.lock().await;
            c.execute_batch(include_str!(
                "../migrations/0033_child_tables_vault_id.down.sql"
            ))
            .expect("application rollback 0033.down");
        }

        // Empreinte post-rollback.
        let c = conn.lock().await;
        for (i, t) in tables.iter().enumerate() {
            assert_eq!(
                table_fingerprint(&c, t),
                schema_before[i],
                "{t} : schéma non restauré à l'identique après rollback 0033"
            );
            assert_eq!(
                index_fingerprint(&c, t),
                index_before[i],
                "{t} : index non restaurés à l'identique après rollback 0033"
            );
        }
        // vault_id doit avoir disparu du schéma restauré.
        let has_vault_id = table_fingerprint(&c, "note_embeddings")
            .iter()
            .any(|(name, ..)| name == "vault_id");
        assert!(
            !has_vault_id,
            "note_embeddings.vault_id doit être absent après rollback"
        );
        // Données préservées.
        let emb_after: i64 = c
            .query_row(
                "SELECT dim FROM note_embeddings WHERE note_id='01D2REVERT00001' AND embedder_id='bge-m3'",
                [], |r| r.get(0),
            )
            .expect("emb post-rollback");
        assert_eq!(
            emb_after, data_before,
            "donnée embedding perdue au rollback"
        );
        // 0033 retiré du registre → ré-applicable.
        let still_applied: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0033_child_tables_vault_id')",
                [], |r| r.get(0),
            )
            .expect("check registre");
        assert!(
            !still_applied,
            "0033 doit être retiré de _schema_migrations après rollback"
        );
    }

    /// Idempotence 0033 au niveau runner : un double `run()` applique 0033 une seule
    /// fois (guard `_schema_migrations`), schéma stable (Step 4 du brief D2).
    #[tokio::test]
    async fn migration_0033_runner_idempotent() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        // open_in_memory brut : on applique tout via run() (0033 inclus).
        run(&conn).await.expect("run() première fois");
        let fp1 = {
            let c = conn.lock().await;
            table_fingerprint(&c, "note_embeddings")
        };
        run(&conn).await.expect("run() seconde fois (idempotence)");
        let c = conn.lock().await;
        assert_eq!(
            table_fingerprint(&c, "note_embeddings"),
            fp1,
            "schéma note_embeddings modifié par un second run() — 0033 non idempotent"
        );
        // Exactement une ligne de registre pour 0033.
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version='0033_child_tables_vault_id'",
                [], |r| r.get(0),
            )
            .expect("count registre 0033");
        assert_eq!(
            n, 1,
            "0033 doit apparaître exactement une fois dans le registre"
        );
    }

    /// Volumétrie 0033 : sur un jeu ≥ ordre LIVE (10 000 notes + embeddings), le backfill
    /// est complet (0 NULL, count préservé) et l'intégrité `notes == notes_fts` tenue
    /// (0033 ne touche pas la FTS) — Step 5 du brief D2.
    #[tokio::test]
    async fn migration_0033_volumetry_10k() {
        const N: usize = 10_000;
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0033(&conn).await;

        // Seed 10k notes + 10k embeddings + FTS 1:1, en une transaction.
        {
            let c = conn.lock().await;
            let tx = c.unchecked_transaction().expect("begin tx seed");
            {
                let mut stmt_note = tx
                    .prepare(
                        "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
                         VALUES (?1, 'main', 'reference', 'live', 1, 0, X'00', 'body')",
                    )
                    .expect("prepare note");
                let mut stmt_emb = tx
                    .prepare(
                        "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at)
                         VALUES (?1, 'bge-m3', X'00000000', 1, NULL, 0)",
                    )
                    .expect("prepare emb");
                for i in 0..N {
                    let id = format!("01VOL{i:020}");
                    stmt_note.execute([&id]).expect("insert note vol");
                    stmt_emb.execute([&id]).expect("insert emb vol");
                }
            }
            tx.execute_batch(
                "INSERT INTO notes_fts (rowid, body_text) SELECT rowid, body_text FROM notes;",
            )
            .expect("seed fts 1:1");
            tx.commit().expect("commit seed");
        }

        let started = std::time::Instant::now();
        apply_migration_0033(&conn).await;
        let elapsed = started.elapsed();
        // Mesure informative (pas d'assertion de temps — bruit en debug).
        eprintln!("migration 0033 sur {N} notes/embeddings : {elapsed:?}");

        let c = conn.lock().await;
        let emb_count: i64 = c
            .query_row("SELECT COUNT(*) FROM note_embeddings", [], |r| r.get(0))
            .expect("count emb");
        assert_eq!(
            emb_count, N as i64,
            "count note_embeddings doit être préservé ({N}), obtenu {emb_count}"
        );
        let nulls: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM note_embeddings WHERE vault_id IS NULL OR vault_id != 'main'",
                [],
                |r| r.get(0),
            )
            .expect("count null/diverge");
        assert_eq!(
            nulls, 0,
            "backfill volumétrie : {nulls} vault_id NULL/divergents"
        );

        let notes_n: i64 = c
            .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .expect("count notes");
        let fts_n: i64 = c
            .query_row("SELECT COUNT(*) FROM notes_fts", [], |r| r.get(0))
            .expect("count fts");
        assert_eq!(
            notes_n, fts_n,
            "intégrité FTS : notes ({notes_n}) != notes_fts ({fts_n}) après 0033"
        );
    }

    // ── Tests migration 0034 — PK composites enfants (C4-1e, Slice D3) ─────────

    /// Applique les migrations jusqu'à (non inclus) 0034 → schéma pré-0034.
    ///
    /// `take_while` sur le nom de version est robuste à l'insertion de migrations
    /// avant 0034 (contrairement à un `take(n)` positionnel).
    async fn apply_migrations_before_0034(conn: &Arc<Mutex<Connection>>) {
        let c = conn.lock().await;
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS _schema_migrations (version TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
        )
        .expect("bootstrap _schema_migrations");
        for (version, sql) in MIGRATIONS
            .iter()
            .take_while(|(v, _)| *v != "0034_child_tables_composite_pk")
        {
            let already: bool = c
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = ?1)",
                    [version],
                    |r| r.get(0),
                )
                .expect("check migration");
            // Skip conditionnel des migrations à extension (0020 vec0) — parité `run()`.
            if !already && extension_available_for(&c, version) {
                c.execute_batch(sql)
                    .unwrap_or_else(|e| panic!("migration {version} : {e}"));
            }
        }
    }

    /// Applique explicitement le batch 0034 (et uniquement lui) sur une DB déjà à 0033.
    async fn apply_migration_0034(conn: &Arc<Mutex<Connection>>) {
        let sql = MIGRATIONS
            .iter()
            .find(|(v, _)| *v == "0034_child_tables_composite_pk")
            .map(|(_, sql)| *sql)
            .expect("0034 enregistrée dans MIGRATIONS");
        let c = conn.lock().await;
        c.execute_batch(sql).expect("application 0034");
    }

    /// Colonnes composant la PK d'une table, ordonnées par l'ordinal `pk` de SQLite
    /// (`pragma_table_info.pk` : 0 = hors PK, 1 = 1ʳᵉ colonne de la PK, etc.).
    fn pk_columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut cols: Vec<(i64, String)> = conn
            .prepare(&format!(
                "SELECT name, pk FROM pragma_table_info('{table}') WHERE pk > 0"
            ))
            .expect("prepare pragma_table_info pk")
            .query_map([], |r| Ok((r.get::<_, i64>(1)?, r.get::<_, String>(0)?)))
            .expect("query pragma_table_info pk")
            .map(|r| r.expect("row pragma_table_info pk"))
            .collect();
        cols.sort_by_key(|(ordinal, _)| *ordinal);
        cols.into_iter().map(|(_, name)| name).collect()
    }

    /// Sème une ligne dans chacune des 3 tables ciblées par 0034 (`note_index`,
    /// `temporal_index`, `note_overrides`) pour un `(vault_id, note_id)` donné.
    /// Schéma pré-0034 = post-0034 sur les colonnes (seule la PK change) → INSERT identique.
    async fn seed_children_pre_0034(conn: &Arc<Mutex<Connection>>, vault_id: &str, note_id: &str) {
        let c = conn.lock().await;
        c.execute(
            "INSERT INTO note_index (note_id, vault_id, locus, bm25_tokens, last_indexed) \
             VALUES (?1, ?2, NULL, 0, 0)",
            rusqlite::params![note_id, vault_id],
        )
        .expect("seed note_index pré-0034");
        c.execute(
            "INSERT INTO temporal_index (note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms) \
             VALUES (?1, ?2, 42, 'created', 'Static', NULL)",
            rusqlite::params![note_id, vault_id],
        )
        .expect("seed temporal_index pré-0034");
        c.execute(
            "INSERT INTO note_overrides (note_id, vault_id, scope_kind, scope_id, override_type, \
             schema_version, payload_toml, created_at, file_relative_path, file_hash) \
             VALUES (?1, ?2, 'vault', ?2, 'trust', 1, 'x = 1', 0, ?3, X'00')",
            rusqlite::params![
                note_id,
                vault_id,
                format!("{vault_id}/{note_id}.trust.toml")
            ],
        )
        .expect("seed note_overrides pré-0034");
    }

    /// Step 1-2 : après 0034, les 3 tables portent la PK composite attendue, le compte de
    /// lignes est préservé (0 perte) et les données sont intactes.
    #[tokio::test]
    async fn migration_0034_c4_1e_pk_composites_effective_and_count_preserved() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0034(&conn).await;
        seed_children_pre_0034(&conn, "main", "01D3PK0000000000000000001").await;
        apply_migration_0034(&conn).await;

        let c = conn.lock().await;

        // PK composite effective par table (ordonnée).
        assert_eq!(
            pk_columns(&c, "note_index"),
            vec!["vault_id", "note_id"],
            "note_index : PK attendue (vault_id, note_id)"
        );
        assert_eq!(
            pk_columns(&c, "temporal_index"),
            vec!["vault_id", "note_id"],
            "temporal_index : PK attendue (vault_id, note_id)"
        );
        assert_eq!(
            pk_columns(&c, "note_overrides"),
            vec![
                "vault_id",
                "note_id",
                "scope_kind",
                "scope_id",
                "override_type"
            ],
            "note_overrides : PK attendue (vault_id, note_id, scope_kind, scope_id, override_type)"
        );

        // Compte préservé (0 perte) + 0 NULL vault_id.
        for table in ["note_index", "temporal_index", "note_overrides"] {
            let count: i64 = c
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .expect("count post-0034");
            assert_eq!(count, 1, "{table} : ligne perdue au recreate (attendu 1)");
            let nulls: i64 = c
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE vault_id IS NULL"),
                    [],
                    |r| r.get(0),
                )
                .expect("count NULL vault_id");
            assert_eq!(nulls, 0, "{table} : {nulls} vault_id NULL après 0034");
        }

        // Données intactes (temporal_index.anchor_ms préservé).
        let anchor: i64 = c
            .query_row(
                "SELECT anchor_ms FROM temporal_index WHERE note_id='01D3PK0000000000000000001' AND vault_id='main'",
                [],
                |r| r.get(0),
            )
            .expect("anchor préservé");
        assert_eq!(anchor, 42, "temporal_index.anchor_ms altéré au recreate");

        // Isolation post-migration : deux vaults, MÊME note_id, coexistent (PK composite).
        c.execute(
            "INSERT INTO temporal_index (note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms) \
             VALUES ('01D3PK0000000000000000001', 'vault-b', 99, 'created', 'Static', NULL)",
            [],
        )
        .expect("insert cross-vault même note_id doit réussir (PK composite)");
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM temporal_index WHERE note_id='01D3PK0000000000000000001'",
                [],
                |r| r.get(0),
            )
            .expect("count cross-vault");
        assert_eq!(
            n, 2,
            "deux lignes temporal_index (main, vault-b) même note_id"
        );

        // 0034 enregistrée.
        let applied: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0034_child_tables_composite_pk')",
                [],
                |r| r.get(0),
            )
            .expect("check 0034");
        assert!(applied, "0034 enregistrée dans _schema_migrations");
    }

    /// Step 3 : réversibilité — up puis rollback (`.down.sql`) restaure le schéma pré-0034
    /// (colonnes/PK/index) ET les données.
    #[tokio::test]
    async fn migration_0034_c4_1e_is_reversible() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0034(&conn).await;
        seed_children_pre_0034(&conn, "main", "01D3REV0000000000000000001").await;

        let tables = ["note_index", "temporal_index", "note_overrides"];

        // Empreinte pré-0034 (schéma + index + une donnée témoin).
        let (schema_before, index_before, anchor_before) = {
            let c = conn.lock().await;
            let schema: Vec<_> = tables.iter().map(|t| table_fingerprint(&c, t)).collect();
            let indexes: Vec<_> = tables.iter().map(|t| index_fingerprint(&c, t)).collect();
            let anchor: i64 = c
                .query_row(
                    "SELECT anchor_ms FROM temporal_index WHERE note_id='01D3REV0000000000000000001'",
                    [],
                    |r| r.get(0),
                )
                .expect("anchor pré");
            (schema, indexes, anchor)
        };

        apply_migration_0034(&conn).await;

        // Rollback manuel via le script documenté `.down.sql`.
        {
            let c = conn.lock().await;
            c.execute_batch(include_str!(
                "../migrations/0034_child_tables_composite_pk.down.sql"
            ))
            .expect("application rollback 0034.down");
        }

        let c = conn.lock().await;
        for (i, t) in tables.iter().enumerate() {
            assert_eq!(
                table_fingerprint(&c, t),
                schema_before[i],
                "{t} : schéma non restauré à l'identique après rollback 0034"
            );
            assert_eq!(
                index_fingerprint(&c, t),
                index_before[i],
                "{t} : index non restaurés à l'identique après rollback 0034"
            );
        }
        // PK d'origine restaurée (note_id seul sur temporal_index).
        assert_eq!(
            pk_columns(&c, "temporal_index"),
            vec!["note_id"],
            "temporal_index : PK d'origine (note_id) doit être restaurée"
        );
        // Donnée préservée.
        let anchor_after: i64 = c
            .query_row(
                "SELECT anchor_ms FROM temporal_index WHERE note_id='01D3REV0000000000000000001'",
                [],
                |r| r.get(0),
            )
            .expect("anchor post-rollback");
        assert_eq!(
            anchor_after, anchor_before,
            "donnée temporal perdue au rollback"
        );
        // 0034 retiré du registre → ré-applicable.
        let still_applied: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0034_child_tables_composite_pk')",
                [],
                |r| r.get(0),
            )
            .expect("check registre");
        assert!(
            !still_applied,
            "0034 doit être retiré de _schema_migrations après rollback"
        );
    }

    /// Step 4 : idempotence runner — un double `run()` applique 0034 une seule fois
    /// (guard `_schema_migrations`), schéma stable.
    #[tokio::test]
    async fn migration_0034_c4_1e_runner_idempotent() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        run(&conn).await.expect("run() première fois");
        let fp1 = {
            let c = conn.lock().await;
            (
                table_fingerprint(&c, "note_index"),
                table_fingerprint(&c, "temporal_index"),
                table_fingerprint(&c, "note_overrides"),
            )
        };
        run(&conn).await.expect("run() seconde fois (idempotence)");
        let c = conn.lock().await;
        assert_eq!(
            (
                table_fingerprint(&c, "note_index"),
                table_fingerprint(&c, "temporal_index"),
                table_fingerprint(&c, "note_overrides"),
            ),
            fp1,
            "schéma modifié par un second run() — 0034 non idempotent"
        );
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version='0034_child_tables_composite_pk'",
                [],
                |r| r.get(0),
            )
            .expect("count registre 0034");
        assert_eq!(
            n, 1,
            "0034 doit apparaître exactement une fois dans le registre"
        );
    }

    /// Volumétrie temporal_index : sur un jeu ≥ ordre LIVE (2206 lignes), le recreate
    /// préserve le compte exact (0 perte).
    #[tokio::test]
    async fn migration_0034_c4_1e_temporal_volumetry_2206() {
        const N: usize = 2206;
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0034(&conn).await;

        {
            let c = conn.lock().await;
            let tx = c.unchecked_transaction().expect("begin tx seed");
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO temporal_index (note_id, vault_id, anchor_ms, anchor_src, doc_kind, valid_until_ms) \
                         VALUES (?1, 'main', ?2, 'created', 'Static', NULL)",
                    )
                    .expect("prepare temporal");
                for i in 0..N {
                    let id = format!("01VOLT{i:019}");
                    stmt.execute(rusqlite::params![id, i as i64])
                        .expect("insert temporal vol");
                }
            }
            tx.commit().expect("commit seed");
        }

        apply_migration_0034(&conn).await;

        let c = conn.lock().await;
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM temporal_index", [], |r| r.get(0))
            .expect("count temporal post-0034");
        assert_eq!(
            count, N as i64,
            "count temporal_index doit être préservé ({N}), obtenu {count}"
        );
        let nulls: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM temporal_index WHERE vault_id IS NULL OR vault_id != 'main'",
                [],
                |r| r.get(0),
            )
            .expect("count null/diverge");
        assert_eq!(
            nulls, 0,
            "volumétrie : {nulls} vault_id NULL/divergents après 0034"
        );
    }

    // ── Tests migration 0035 — redirect_table PK composite (Groupe B, M4) ──────

    /// Applique les migrations jusqu'à (non inclus) 0035 → schéma pré-0035
    /// (`redirect_table` à PK globale `title_slug`, migration 0010).
    async fn apply_migrations_before_0035(conn: &Arc<Mutex<Connection>>) {
        let c = conn.lock().await;
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS _schema_migrations (version TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
        )
        .expect("bootstrap _schema_migrations");
        for (version, sql) in MIGRATIONS
            .iter()
            .take_while(|(v, _)| *v != "0035_redirect_table_vault_id")
        {
            let already: bool = c
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = ?1)",
                    [version],
                    |r| r.get(0),
                )
                .expect("check migration");
            if !already && extension_available_for(&c, version) {
                c.execute_batch(sql)
                    .unwrap_or_else(|e| panic!("migration {version} : {e}"));
            }
        }
    }

    /// Applique explicitement le batch 0035 (et uniquement lui) sur une DB déjà à 0034.
    async fn apply_migration_0035(conn: &Arc<Mutex<Connection>>) {
        let sql = MIGRATIONS
            .iter()
            .find(|(v, _)| *v == "0035_redirect_table_vault_id")
            .map(|(_, sql)| *sql)
            .expect("0035 enregistrée dans MIGRATIONS");
        let c = conn.lock().await;
        c.execute_batch(sql).expect("application 0035");
    }

    /// Sème une ligne `redirect_table` au schéma pré-0035 (PK globale `title_slug`,
    /// sans colonne `vault_id`).
    async fn seed_redirect_pre_0035(conn: &Arc<Mutex<Connection>>, slug: &str, ulid: &str) {
        let c = conn.lock().await;
        c.execute(
            "INSERT INTO redirect_table (title_slug, ulid, renamed_at) VALUES (?1, ?2, 42)",
            rusqlite::params![slug, ulid],
        )
        .expect("seed redirect_table pré-0035");
    }

    /// Step 1-2 : après 0035, `redirect_table` porte la PK composite
    /// `(vault_id, title_slug)`, le compte est préservé (0 perte), les lignes legacy sont
    /// backfillées `vault_id='main'` et les données (ulid/renamed_at) intactes.
    #[tokio::test]
    async fn migration_0035_m4_pk_composite_effective_and_backfill_main() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0035(&conn).await;
        seed_redirect_pre_0035(&conn, "titre-legacy", "01D3REDIR000000000000000001").await;
        apply_migration_0035(&conn).await;

        let c = conn.lock().await;

        // PK composite effective (ordonnée).
        assert_eq!(
            pk_columns(&c, "redirect_table"),
            vec!["vault_id", "title_slug"],
            "redirect_table : PK attendue (vault_id, title_slug)"
        );

        // Compte préservé (0 perte) + 0 NULL vault_id.
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM redirect_table", [], |r| r.get(0))
            .expect("count post-0035");
        assert_eq!(
            count, 1,
            "redirect_table : ligne perdue au recreate (attendu 1)"
        );
        let nulls: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM redirect_table WHERE vault_id IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("count NULL vault_id");
        assert_eq!(
            nulls, 0,
            "redirect_table : {nulls} vault_id NULL après 0035"
        );

        // Backfill : la ligne legacy est rattachée au vault 'main', données intactes.
        let (vault_id, ulid, renamed_at): (String, String, i64) = c
            .query_row(
                "SELECT vault_id, ulid, renamed_at FROM redirect_table WHERE title_slug='titre-legacy'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("row legacy post-0035");
        assert_eq!(
            vault_id, "main",
            "ligne legacy doit être backfillée vault_id='main'"
        );
        assert_eq!(
            ulid, "01D3REDIR000000000000000001",
            "ulid legacy altéré au recreate"
        );
        assert_eq!(renamed_at, 42, "renamed_at legacy altéré au recreate");

        // Isolation post-migration : deux vaults, MÊME title_slug, coexistent (PK composite).
        c.execute(
            "INSERT INTO redirect_table (vault_id, title_slug, ulid, renamed_at) \
             VALUES ('vault-b', 'titre-legacy', '01D3REDIR000000000000000002', 99)",
            [],
        )
        .expect("insert cross-vault même title_slug doit réussir (PK composite)");
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM redirect_table WHERE title_slug='titre-legacy'",
                [],
                |r| r.get(0),
            )
            .expect("count cross-vault");
        assert_eq!(n, 2, "deux lignes redirect (main, vault-b) même title_slug");

        // idx_redirect_ulid recréé (perdu au DROP de l'ancienne table).
        let has_idx: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_redirect_ulid')",
                [],
                |r| r.get(0),
            )
            .expect("check idx_redirect_ulid");
        assert!(has_idx, "idx_redirect_ulid doit être recréé après 0035");

        // 0035 enregistrée.
        let applied: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0035_redirect_table_vault_id')",
                [],
                |r| r.get(0),
            )
            .expect("check 0035");
        assert!(applied, "0035 enregistrée dans _schema_migrations");
    }

    /// Step 3 : réversibilité — up puis rollback (`.down.sql`) restaure le schéma pré-0035
    /// (PK `title_slug`, sans colonne `vault_id`) ET la donnée témoin.
    #[tokio::test]
    async fn migration_0035_m4_is_reversible() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0035(&conn).await;
        seed_redirect_pre_0035(&conn, "titre-rev", "01D3REV0REDIR0000000000001").await;

        // Empreinte pré-0035 (schéma + index).
        let (schema_before, index_before) = {
            let c = conn.lock().await;
            (
                table_fingerprint(&c, "redirect_table"),
                index_fingerprint(&c, "redirect_table"),
            )
        };

        apply_migration_0035(&conn).await;

        // Rollback manuel via le script documenté `.down.sql`.
        {
            let c = conn.lock().await;
            c.execute_batch(include_str!(
                "../migrations/0035_redirect_table_vault_id.down.sql"
            ))
            .expect("application rollback 0035.down");
        }

        let c = conn.lock().await;
        assert_eq!(
            table_fingerprint(&c, "redirect_table"),
            schema_before,
            "redirect_table : schéma non restauré à l'identique après rollback 0035"
        );
        assert_eq!(
            index_fingerprint(&c, "redirect_table"),
            index_before,
            "redirect_table : index non restaurés à l'identique après rollback 0035"
        );
        // PK d'origine restaurée (title_slug seul).
        assert_eq!(
            pk_columns(&c, "redirect_table"),
            vec!["title_slug"],
            "redirect_table : PK d'origine (title_slug) doit être restaurée"
        );
        // Donnée préservée (ulid témoin).
        let ulid_after: String = c
            .query_row(
                "SELECT ulid FROM redirect_table WHERE title_slug='titre-rev'",
                [],
                |r| r.get(0),
            )
            .expect("ulid post-rollback");
        assert_eq!(
            ulid_after, "01D3REV0REDIR0000000000001",
            "donnée redirect perdue au rollback"
        );
        // 0035 retiré du registre → ré-applicable.
        let still_applied: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0035_redirect_table_vault_id')",
                [],
                |r| r.get(0),
            )
            .expect("check registre");
        assert!(
            !still_applied,
            "0035 doit être retiré de _schema_migrations après rollback"
        );
    }

    // ── Tests migration 0039 — FK composite sur les tables filles D2 (item 01KXV6PJ0X) ──

    /// Applique les migrations jusqu'à (non inclus) 0039 → schéma pré-0039 (état 0038).
    async fn apply_migrations_before_0039(conn: &Arc<Mutex<Connection>>) {
        let c = conn.lock().await;
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS _schema_migrations (version TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);",
        )
        .expect("bootstrap _schema_migrations");
        for (version, sql) in MIGRATIONS
            .iter()
            .take_while(|(v, _)| *v != "0039_child_tables_composite_fk")
        {
            let already: bool = c
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version = ?1)",
                    [version],
                    |r| r.get(0),
                )
                .expect("check migration");
            if !already && extension_available_for(&c, version) {
                c.execute_batch(sql)
                    .unwrap_or_else(|e| panic!("migration {version} : {e}"));
            }
        }
    }

    /// Applique explicitement le batch 0039 (et uniquement lui) sur une DB déjà à 0038.
    async fn apply_migration_0039(conn: &Arc<Mutex<Connection>>) {
        let sql = MIGRATIONS
            .iter()
            .find(|(v, _)| *v == "0039_child_tables_composite_fk")
            .map(|(_, sql)| *sql)
            .expect("0039 enregistrée dans MIGRATIONS");
        let c = conn.lock().await;
        c.execute_batch(sql).expect("application 0039");
    }

    /// Nombre de contraintes FK DISTINCTES déclarées sur `table`.
    ///
    /// `pragma_foreign_key_list` émet UNE ligne par colonne de FK ; un FK composite à deux
    /// colonnes `(vault_id, note_id)` produit donc deux lignes partageant le même `id` de
    /// contrainte. On compte `DISTINCT id` pour dénombrer les contraintes, pas les colonnes.
    fn fk_count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(
            &format!("SELECT COUNT(DISTINCT id) FROM pragma_foreign_key_list('{table}')"),
            [],
            |r| r.get(0),
        )
        .expect("pragma_foreign_key_list count")
    }

    /// Nombre de violations d'intégrité référentielle globales (`PRAGMA foreign_key_check`).
    fn foreign_key_violations(conn: &Connection) -> i64 {
        let mut stmt = conn
            .prepare("PRAGMA foreign_key_check")
            .expect("prepare foreign_key_check");
        stmt.query_map([], |_| Ok(()))
            .expect("query foreign_key_check")
            .count() as i64
    }

    /// Sème une note parente `main` + une ligne fille (schéma post-0038, avec `vault_id`)
    /// dans chacune des 3 tables D2 ciblées par 0039, pour un `note_id` donné.
    async fn seed_note_and_d2_children_main(conn: &Arc<Mutex<Connection>>, note_id: &str) {
        let c = conn.lock().await;
        c.execute(
            "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
             VALUES (?1, 'main', 'reference', 'live', 1, 0, X'00', 'body')",
            [note_id],
        )
        .expect("seed note parente main");
        c.execute(
            "INSERT INTO note_audit_trail (note_id, vault_id, event_type, actor_kind, actor_id, occurred_at)
             VALUES (?1, 'main', 'created', 'system', 'test', 0)",
            [note_id],
        )
        .expect("seed audit main");
        c.execute(
            "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at, vault_id)
             VALUES (?1, 'bge-m3', X'00000000', 1, NULL, 0, 'main')",
            [note_id],
        )
        .expect("seed embedding main");
        c.execute(
            "INSERT INTO note_history (note_id, from_version, to_version, diff_text, committed_at, vault_id)
             VALUES (?1, 0, 1, 'diff', 0, 'main')",
            [note_id],
        )
        .expect("seed history main");
    }

    /// 0039 : après recreate, (a) les 3 tables portent un FK composite, (b) les données
    /// existantes sont préservées, (c) `foreign_key_check` est propre (0 orphelin sur données
    /// réalistes), (d) le FK est ACTIF (une insertion enfant orpheline est rejetée), (e) une
    /// insertion enfant avec parent valide reste acceptée.
    #[tokio::test]
    async fn migration_0039_composite_fk_effective_data_preserved_no_orphan() {
        let d2 = ["note_audit_trail", "note_embeddings", "note_history"];
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0039(&conn).await;
        seed_note_and_d2_children_main(&conn, "01FK0039PARENT0001").await;

        // Pré-0039 : aucun FK sur les 3 tables (0032 les a retirés).
        {
            let c = conn.lock().await;
            for t in d2 {
                assert_eq!(fk_count(&c, t), 0, "{t} : aucun FK attendu avant 0039");
            }
        }

        apply_migration_0039(&conn).await;

        let c = conn.lock().await;

        // (a) FK composite présent sur chacune des 3 tables.
        for t in d2 {
            assert_eq!(fk_count(&c, t), 1, "{t} : FK composite absent après 0039");
        }

        // (b) Données préservées (les 3 lignes filles survivent au recreate).
        for t in d2 {
            let n: i64 = c
                .query_row(
                    &format!("SELECT COUNT(*) FROM {t} WHERE note_id = '01FK0039PARENT0001' AND vault_id = 'main'"),
                    [],
                    |r| r.get(0),
                )
                .expect("count fille préservée");
            assert_eq!(n, 1, "{t} : ligne fille perdue au recreate 0039");
        }

        // (c) Aucune violation référentielle sur les données existantes.
        assert_eq!(
            foreign_key_violations(&c),
            0,
            "foreign_key_check doit être propre après 0039 (0 orphelin)"
        );

        // (d) FK ACTIF : une insertion enfant orpheline (note_id sans parent) est rejetée.
        c.execute_batch("PRAGMA foreign_keys=ON;")
            .expect("enable foreign_keys");
        let orphan_audit = c.execute(
            "INSERT INTO note_audit_trail (note_id, vault_id, event_type, actor_kind, actor_id, occurred_at)
             VALUES ('01FK0039ORPHAN0001', 'main', 'x', 'system', 's', 0)",
            [],
        );
        assert!(
            orphan_audit.is_err(),
            "note_audit_trail : insertion orpheline doit être rejetée par le FK composite"
        );
        let orphan_emb = c.execute(
            "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at, vault_id)
             VALUES ('01FK0039ORPHAN0001', 'bge-m3', X'00000000', 1, NULL, 0, 'main')",
            [],
        );
        assert!(
            orphan_emb.is_err(),
            "note_embeddings : insertion orpheline doit être rejetée par le FK composite"
        );
        let orphan_hist = c.execute(
            "INSERT INTO note_history (note_id, from_version, to_version, diff_text, committed_at, vault_id)
             VALUES ('01FK0039ORPHAN0001', 0, 1, 'd', 0, 'main')",
            [],
        );
        assert!(
            orphan_hist.is_err(),
            "note_history : insertion orpheline doit être rejetée par le FK composite"
        );

        // Contre-preuve du FK : même ULID sur un AUTRE vault (parent absent) est aussi rejeté
        // (le FK est composite `(vault_id, note_id)`, pas id-only).
        let cross_vault_orphan = c.execute(
            "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at, vault_id)
             VALUES ('01FK0039PARENT0001', 'bge-m3', X'00000000', 1, NULL, 0, 'vault-b')",
            [],
        );
        assert!(
            cross_vault_orphan.is_err(),
            "note_embeddings : (vault-b, 01FK0039PARENT0001) sans note parente doit être rejeté"
        );

        // (e) Insertion enfant avec parent valide reste acceptée.
        c.execute(
            "INSERT INTO note_embeddings (note_id, embedder_id, vector, dim, model_version, computed_at, vault_id)
             VALUES ('01FK0039PARENT0001', 'bge-small', X'00000000', 1, NULL, 0, 'main')",
            [],
        )
        .expect("insertion enfant avec parent valide doit réussir");
    }

    /// Réversibilité 0039 : up → down → up. Le `.down` restaure exactement le schéma
    /// post-0033 (colonnes/PK/index identiques, FK retiré), préserve les données, retire la
    /// ligne de registre ; le re-up repose le FK. Round-trip prouvé.
    #[tokio::test]
    async fn migration_0039_is_reversible_round_trip() {
        let d2 = ["note_audit_trail", "note_embeddings", "note_history"];
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        apply_migrations_before_0039(&conn).await;
        seed_note_and_d2_children_main(&conn, "01FK0039REVERT0001").await;

        // Empreinte pré-0039 (schéma + index) des 3 tables.
        let (schema_before, index_before) = {
            let c = conn.lock().await;
            let schema: Vec<_> = d2.iter().map(|t| table_fingerprint(&c, t)).collect();
            let indexes: Vec<_> = d2.iter().map(|t| index_fingerprint(&c, t)).collect();
            (schema, indexes)
        };

        // UP : le FK apparaît sur les 3 tables.
        apply_migration_0039(&conn).await;
        {
            let c = conn.lock().await;
            for t in d2 {
                assert_eq!(fk_count(&c, t), 1, "{t} : FK attendu après up 0039");
            }
        }

        // DOWN : rollback manuel documenté.
        {
            let c = conn.lock().await;
            c.execute_batch(include_str!(
                "../migrations/0039_child_tables_composite_fk.down.sql"
            ))
            .expect("application rollback 0039.down");
        }

        {
            let c = conn.lock().await;
            for (i, t) in d2.iter().enumerate() {
                assert_eq!(
                    table_fingerprint(&c, t),
                    schema_before[i],
                    "{t} : schéma non restauré à l'identique après rollback 0039"
                );
                assert_eq!(
                    index_fingerprint(&c, t),
                    index_before[i],
                    "{t} : index non restaurés à l'identique après rollback 0039"
                );
                assert_eq!(
                    fk_count(&c, t),
                    0,
                    "{t} : FK doit être retiré après rollback"
                );
            }
            // Donnée préservée (témoin embedding).
            let dim_after: i64 = c
                .query_row(
                    "SELECT dim FROM note_embeddings WHERE note_id='01FK0039REVERT0001' AND embedder_id='bge-m3'",
                    [],
                    |r| r.get(0),
                )
                .expect("emb post-rollback");
            assert_eq!(dim_after, 1, "donnée embedding perdue au rollback 0039");
            // Registre : 0039 retiré → ré-applicable.
            let still: bool = c
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0039_child_tables_composite_fk')",
                    [],
                    |r| r.get(0),
                )
                .expect("check registre 0039");
            assert!(!still, "0039 doit être retiré du registre après rollback");
        }

        // RE-UP : le FK est reposé, registre ré-inscrit.
        apply_migration_0039(&conn).await;
        let c = conn.lock().await;
        for t in d2 {
            assert_eq!(
                fk_count(&c, t),
                1,
                "{t} : FK doit être reposé au re-up 0039"
            );
        }
        let reapplied: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0039_child_tables_composite_fk')",
                [],
                |r| r.get(0),
            )
            .expect("check registre 0039 re-up");
        assert!(reapplied, "0039 doit être ré-inscrit au registre au re-up");
    }

    /// Idempotence 0039 au niveau runner : double `run()` = une seule application, schéma
    /// stable (FK présent une fois), une unique ligne de registre.
    #[tokio::test]
    async fn migration_0039_runner_idempotent() {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));
        run(&conn).await.expect("run() première fois");
        run(&conn).await.expect("run() seconde fois (idempotence)");
        let c = conn.lock().await;
        for t in ["note_audit_trail", "note_embeddings", "note_history"] {
            assert_eq!(
                fk_count(&c, t),
                1,
                "{t} : exactement un FK après double run() (0039 non idempotent sinon)"
            );
        }
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version='0039_child_tables_composite_fk'",
                [],
                |r| r.get(0),
            )
            .expect("count registre 0039");
        assert_eq!(
            n, 1,
            "0039 doit apparaître exactement une fois dans le registre"
        );
    }

    /// Réversibilité 0043 : up → down → up. Le `.down.sql` retire l'index
    /// `idx_notes_roles` puis les colonnes `role_kind`/`role_status`, et déréférence la
    /// migration du registre ; le re-up (fichier `0043.sql` direct) les repose. Le runner
    /// étant forward-only (le `.down` n'est jamais rejoué seul, migrations.rs run()), on le
    /// charge explicitement via `include_str!` + `execute_batch`, sur le modèle de la
    /// réversibilité 0039. Ce test exerce la méthode de revert : un chemin de
    /// rollback non exercé est un chemin mort.
    #[tokio::test]
    async fn migration_0043_is_reversible_round_trip() {
        fn columns(conn: &Connection) -> Vec<String> {
            conn.prepare("SELECT name FROM pragma_table_info('notes')")
                .expect("pragma_table_info(notes)")
                .query_map([], |r| r.get::<_, String>(0))
                .expect("query_map table_info")
                .filter_map(std::result::Result::ok)
                .collect()
        }
        fn has_index(conn: &Connection, name: &str) -> bool {
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .expect("sqlite_master index count")
                > 0
        }

        let conn = Arc::new(Mutex::new(Connection::open_in_memory().expect("in-memory")));

        // UP : chaîne complète jusqu'à 0043 incluse.
        run(&conn).await.expect("run() jusqu'à 0043");
        {
            let c = conn.lock().await;
            let cols = columns(&c);
            assert!(
                cols.contains(&"role_kind".to_string()),
                "role_kind absent après up 0043, cols={cols:?}"
            );
            assert!(
                cols.contains(&"role_status".to_string()),
                "role_status absent après up 0043"
            );
            assert!(
                has_index(&c, "idx_notes_roles"),
                "idx_notes_roles absent après up 0043"
            );
        }

        // DOWN : rollback manuel documenté, chargé explicitement (runner forward-only).
        {
            let c = conn.lock().await;
            c.execute_batch(include_str!(
                "../migrations/0043_project_map_roles.down.sql"
            ))
            .expect("application rollback 0043.down");
            let cols = columns(&c);
            assert!(
                !cols.contains(&"role_kind".to_string()),
                "role_kind subsiste après down 0043"
            );
            assert!(
                !cols.contains(&"role_status".to_string()),
                "role_status subsiste après down 0043"
            );
            assert!(
                !has_index(&c, "idx_notes_roles"),
                "idx_notes_roles subsiste après down 0043"
            );
            let still: bool = c
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0043_project_map_roles')",
                    [],
                    |r| r.get(0),
                )
                .expect("check registre 0043 après down");
            assert!(!still, "0043 doit être retiré du registre après down");
        }

        // RE-UP : le fichier de migration up direct repose colonnes + index + registre.
        {
            let c = conn.lock().await;
            c.execute_batch(include_str!("../migrations/0043_project_map_roles.sql"))
                .expect("ré-application 0043 up");
            let cols = columns(&c);
            assert!(
                cols.contains(&"role_kind".to_string()),
                "role_kind absent après re-up 0043"
            );
            assert!(
                cols.contains(&"role_status".to_string()),
                "role_status absent après re-up 0043"
            );
            assert!(
                has_index(&c, "idx_notes_roles"),
                "idx_notes_roles absent après re-up 0043"
            );
            let reapplied: bool = c
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM _schema_migrations WHERE version='0043_project_map_roles')",
                    [],
                    |r| r.get(0),
                )
                .expect("check registre 0043 re-up");
            assert!(reapplied, "0043 doit être ré-inscrit au registre au re-up");
        }
    }

    /// Non-régression suppression (cœur de l'item) : sur une DB complète (C12
    /// `foreign_keys=ON` + FK 0039 actif), supprimer une note ayant des lignes dans les 3
    /// tables filles D2 réussit END-TO-END — la note ET ses enfants sont purgés, 0 violation
    /// FK après coup. Preuve que le FK `ON DELETE CASCADE` + cascade manuelle ne régressent
    /// pas la suppression.
    #[tokio::test]
    async fn migration_0039_delete_note_with_d2_children_succeeds_end_to_end() {
        use crate::SqliteIndex;

        let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
        let note_id = "01FK0039DELETE0001";

        // Note parente (main) + 1 ligne fille dans chacune des 3 tables D2 (+ note_index,
        // enfant non-FK, pour vérifier la cascade manuelle conjointe).
        {
            let conn = idx.conn.lock().await;
            conn.execute(
                "INSERT INTO notes (id, vault_id, section, status, schema_version, created, content_hash, body_text)
                 VALUES (?1, 'main', 'reference', 'live', 1, 0, X'00', 'body à supprimer')",
                [note_id],
            )
            .expect("insert note parente");
            // Synchroniser l'entrée FTS5 external-content (`content=notes`) : `delete_note_from_index`
            // exécute `DELETE FROM notes_fts WHERE rowid=?` ; sans entrée FTS correspondante, la
            // suppression external-content lève « database disk image is malformed ».
            conn.execute(
                "INSERT INTO notes_fts (rowid, body_text) SELECT rowid, body_text FROM notes WHERE id = ?1 AND vault_id = 'main'",
                [note_id],
            )
            .expect("seed notes_fts");
        }
        for table in [
            "note_audit_trail",
            "note_embeddings",
            "note_history",
            "note_index",
        ] {
            idx.seed_child_row_for_test(table, "main", note_id)
                .await
                .unwrap_or_else(|e| panic!("seed {table} : {e}"));
        }

        // Suppression end-to-end : doit réussir (Ok(true)).
        let deleted = idx
            .delete_note_from_index("main", note_id)
            .await
            .expect("delete_note_from_index ne doit pas régresser avec le FK 0039");
        assert!(
            deleted,
            "la suppression doit rapporter true (note trouvée + purgée)"
        );

        // Enfants purgés (D2 par cascade FK + manuelle ; note_index par cascade manuelle).
        for table in [
            "note_audit_trail",
            "note_embeddings",
            "note_history",
            "note_index",
        ] {
            let remaining = idx
                .count_child_rows_for_test(table, "main", note_id)
                .await
                .unwrap_or_else(|e| panic!("count {table} : {e}"));
            assert_eq!(
                remaining, 0,
                "{table} : ligne fille non purgée après suppression"
            );
        }

        // 0 violation référentielle résiduelle.
        let conn = idx.conn.lock().await;
        assert_eq!(
            foreign_key_violations(&conn),
            0,
            "foreign_key_check doit être propre après la suppression end-to-end"
        );
    }
}
