//! `gradatum-admin project-map export-features --json` — projection JSON des cartes-feature.
//!
//! Produit un tableau JSON trié par identifiant F-XX croissant, destiné à alimenter
//! le gate CI T4 (miroir-site) ou un audit complet (`--include-dropped`).
//!
//! ## Architecture
//!
//! - [`crate::project_map_export::export_features_from_conn`] : logique SQL pure, prend une connexion rusqlite.
//!   Testable avec `Connection::open_in_memory()`.
//! - [`crate::project_map_export::export_features`] : wrapper async pour l'usage CLI (`spawn_blocking`).
//!
//! ## Projection partagée (DRY)
//!
//! La transformation `notes brutes → Vec<FeatureEntry>` est déléguée à
//! [`gradatum_core::project_map::project_map_feature_entries`] — SSOT partagée
//! avec le handler HTTP `GET /api/v1/project-map/export-features`.
//!
//! ## Filtrage
//!
//! - Seules les cartes avec `[[feature:F-XX]]` sont considérées (exclut les
//!   cartes changelog historiques sans rôle feature).
//! - Miroir-site (`include_dropped = false`) : exclut uniquement `release:dropped`.
//!   Les cartes `version:<proj>/backlog` sont **incluses** (Règle A NOMENCLATURE
//!   §10e) avec `version = "vX.Y.Z"` — le discriminant d'exclusion est le champ
//!   `release`, pas la nullité de version.
//! - Audit complet (`include_dropped = true`) : toutes les cartes-feature.
//! - Les notes lifecycle `status='downgraded'`/`'garbage'` sont toujours exclues
//!   (filtre SQL, indépendant du flag).
//!
//! ## Tri
//!
//! Tri par identifiant `F-XX` croissant (numérique sur la partie `\d{2,3}`).

use anyhow::{Context, Result};

// Réexportation des types partagés pour compatibilité avec les appelants existants.
pub use gradatum_core::project_map::{ExportOptions, FeatureEntry};

/// Logique SQL pure — prend une connexion existante, testable avec
/// `Connection::open_in_memory()`.
///
/// Interroge `notes` section `'project-map'` (exclut `downgraded`/`garbage`),
/// collecte le body_text et le title de chaque note, et délègue la projection
/// à [`gradatum_core::project_map::project_map_feature_entries`].
///
/// # Errors
///
/// - Si la requête SQL échoue.
/// - Si une ligne ne peut pas être lue.
pub fn export_features_from_conn(
    conn: &rusqlite::Connection,
    vault: &str,
    opts: ExportOptions,
) -> Result<Vec<FeatureEntry>> {
    // Même requête que project_scope_from_conn : section project-map, hors lifecycle.
    // On ne filtre PAS sur [[project:]] ici — l'export est multi-projets (vault-scope).
    let sql = "
        SELECT n.body_text, n.title
        FROM notes n
        WHERE n.vault_id = ?1
          AND n.section = 'project-map'
          AND n.status != 'downgraded'
          AND n.status != 'garbage'
        ORDER BY n.created DESC
    ";

    let mut stmt = conn
        .prepare(sql)
        .context("préparation requête export-features")?;

    // Collecte (body_text, title).
    // `title` est nullable en prod (migration 0005 : `ADD COLUMN title TEXT` sans NOT NULL).
    // Les cartes sans H1 extrait ont `title = NULL` → lire `Option<String>`,
    // puis `.unwrap_or_default()` → `""` (dégradation gracieuse, pas d'erreur rusqlite).
    let notes: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![vault], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .context("exécution requête export-features")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("lecture lignes export-features")?;

    // Délégation de la projection pure à gradatum-core (SSOT partagée avec le serveur).
    Ok(gradatum_core::project_map::project_map_feature_entries(
        &notes, opts,
    ))
}

/// Async wrapper : ouvre l'index SQLite et délègue à `export_features_from_conn`.
///
/// `root` est le répertoire racine Gradatum (ex. `/var/lib/gradatum`).
///
/// # Errors
///
/// - Si `index.db` est introuvable.
/// - Si la connexion SQLite échoue.
/// - Si la requête SQL échoue.
pub async fn export_features(
    root: &std::path::Path,
    vault: &str,
    opts: ExportOptions,
) -> Result<Vec<FeatureEntry>> {
    use gradatum_core::paths::vault_index_path;

    let db_path = vault_index_path(root);
    if !db_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le serveur doit avoir démarré au moins une fois",
            db_path.display()
        );
    }

    let db_path = db_path.clone();
    let vault = vault.to_string();

    // spawn_blocking : rusqlite est synchrone, safe depuis un thread tokio current_thread.
    tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open(&db_path)
            .with_context(|| format!("ouverture index.db : {}", db_path.display()))?;
        export_features_from_conn(&conn, &vault, opts)
    })
    .await
    .context("spawn_blocking export_features")?
}

// ─── Tests unitaires ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    fn create_schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                vault_id TEXT NOT NULL,
                section TEXT NOT NULL,
                body_text TEXT NOT NULL,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                created TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .expect("création table test");
    }

    fn insert_note(
        conn: &Connection,
        id: &str,
        vault: &str,
        section: &str,
        body: &str,
        title: &str,
        status: &str,
    ) {
        conn.execute(
            "INSERT INTO notes (id, vault_id, section, body_text, title, status) VALUES (?1,?2,?3,?4,?5,?6)",
            rusqlite::params![id, vault, section, body, title, status],
        )
        .expect("insert note test");
    }

    // ── Règle A : backlog inclus dans le miroir-site avec version "vX.Y.Z" ───

    /// Carte backlog → `version == "vX.Y.Z"` (plus None).
    #[test]
    fn backlog_card_exports_with_vxyz_version() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);
        insert_note(
            &conn,
            "f-backlog",
            "main",
            "project-map",
            "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[release:planned]] [[version:gradatum/backlog]]",
            "F-50 backlog",
            "live",
        );

        let entries =
            export_features_from_conn(&conn, "main", ExportOptions::default()).expect("export");
        assert_eq!(entries.len(), 1, "carte backlog incluse : {entries:?}");
        assert_eq!(entries[0].version, Some("vX.Y.Z".to_string()));
        assert_eq!(entries[0].feature, "F-50");
    }

    /// Carte backlog → **incluse** dans le miroir-site par défaut.
    #[test]
    fn backlog_card_included_in_default_mirror() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);
        insert_note(
            &conn,
            "f-backlog",
            "main",
            "project-map",
            "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[release:planned]] [[version:gradatum/backlog]]",
            "F-50 backlog",
            "live",
        );

        let entries =
            export_features_from_conn(&conn, "main", ExportOptions::default()).expect("export");
        assert!(
            !entries.is_empty(),
            "carte backlog doit être incluse par défaut"
        );
    }

    /// Carte `release:dropped` → **exclue** par défaut.
    #[test]
    fn dropped_card_excluded_by_default() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);
        insert_note(
            &conn,
            "f-dropped",
            "main",
            "project-map",
            "[[feature:F-51]] [[project:gradatum]] [[status:OBSOLETE]] [[kind:FEATURE]] [[release:dropped]] [[version:gradatum/0.6.0]]",
            "F-51 dropped",
            "live",
        );

        let entries =
            export_features_from_conn(&conn, "main", ExportOptions::default()).expect("export");
        assert!(
            entries.is_empty(),
            "carte dropped exclue par défaut : {entries:?}"
        );
    }

    /// Carte `release:dropped` → **incluse** avec `--include-dropped`.
    #[test]
    fn dropped_card_included_with_include_dropped() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);
        insert_note(
            &conn,
            "f-dropped",
            "main",
            "project-map",
            "[[feature:F-51]] [[project:gradatum]] [[status:OBSOLETE]] [[kind:FEATURE]] [[release:dropped]] [[version:gradatum/0.6.0]]",
            "F-51 dropped",
            "live",
        );

        let entries = export_features_from_conn(
            &conn,
            "main",
            ExportOptions {
                include_dropped: true,
            },
        )
        .expect("export");
        assert_eq!(
            entries.len(),
            1,
            "carte dropped incluse avec include_dropped"
        );
        assert_eq!(entries[0].feature, "F-51");
    }

    /// Carte `gradatum/0.6.3` released → `version == "v0.6.3"` (inchangé).
    #[test]
    fn concrete_version_exports_with_v_prefix() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);
        insert_note(
            &conn,
            "f-released",
            "main",
            "project-map",
            "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] [[release:released]] [[version:gradatum/0.6.3]]",
            "F-37 released",
            "live",
        );

        let entries =
            export_features_from_conn(&conn, "main", ExportOptions::default()).expect("export");
        assert_eq!(entries.len(), 1, "carte released incluse");
        assert_eq!(entries[0].version, Some("v0.6.3".to_string()));
    }

    /// Sort order: F-37 before F-50 (ascending numeric feature ID).
    #[test]
    fn backlog_sorts_after_numeric_versions_by_feature_id() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);
        // F-50 backlog
        insert_note(
            &conn,
            "f-50",
            "main",
            "project-map",
            "[[feature:F-50]] [[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] [[release:planned]] [[version:gradatum/backlog]]",
            "F-50 backlog",
            "live",
        );
        // F-37 released
        insert_note(
            &conn,
            "f-37",
            "main",
            "project-map",
            "[[feature:F-37]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] [[release:released]] [[version:gradatum/0.6.3]]",
            "F-37 released",
            "live",
        );

        let entries =
            export_features_from_conn(&conn, "main", ExportOptions::default()).expect("export");
        assert_eq!(entries.len(), 2);
        // Tri F-XX numérique croissant : F-37 avant F-50.
        assert_eq!(entries[0].feature, "F-37");
        assert_eq!(entries[0].version, Some("v0.6.3".to_string()));
        assert_eq!(entries[1].feature, "F-50");
        assert_eq!(entries[1].version, Some("vX.Y.Z".to_string()));
    }

    #[test]
    fn changelog_card_without_feature_is_excluded() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);

        // Carte changelog sans [[feature:]] — doit être ignorée
        insert_note(
            &conn,
            "n1",
            "main",
            "project-map",
            "[[project:gradatum]] [[status:DONE]] [[kind:FIX]] [[version:gradatum/0.5.2]]\n\nFix.",
            "Fix changelog",
            "live",
        );

        let entries =
            export_features_from_conn(&conn, "main", ExportOptions::default()).expect("export");
        assert!(entries.is_empty(), "carte changelog exclue : {entries:?}");
    }

    #[test]
    fn empty_vault_returns_empty_vec() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);

        let entries =
            export_features_from_conn(&conn, "main", ExportOptions::default()).expect("export");
        assert!(entries.is_empty());
    }
}
