//! `gradatum-admin project-map export-features --json` — JSON projection of feature cards.
//!
//! Produces a JSON array sorted by ascending feature identifier. It feeds two consumers:
//! the continuous-integration check that the published website mirrors the vault, and a
//! full audit when `--include-dropped` is passed.
//!
//! ## Layout
//!
//! - [`crate::project_map_export::export_features_from_conn`] holds the pure SQL logic
//!   and takes an open `rusqlite` connection, so it can be tested against
//!   `Connection::open_in_memory()`.
//! - [`crate::project_map_export::export_features`] is the async wrapper used by the CLI,
//!   running the blocking work on a dedicated thread.
//!
//! ## Shared projection
//!
//! Turning raw notes into `Vec<FeatureEntry>` is delegated to
//! [`gradatum_core::project_map::project_map_feature_entries`], which is the single
//! implementation shared with the HTTP handler behind
//! `GET /api/v1/project-map/export-features`.
//!
//! ## Filtering
//!
//! - Only cards carrying a `[[feature:…]]` link are considered, which leaves out
//!   historical changelog cards that play no feature role.
//! - Website mirror (`include_dropped = false`): `release:dropped` cards are excluded,
//!   and so is every card whose kind is not `FEATURE`. Cards pinned to the sentinel
//!   `version:<project>/backlog` are **kept**, and exported with a placeholder version
//!   string rather than a null one — exclusion is decided by the `release` and `kind`
//!   links, never by the absence of a concrete version.
//! - Full audit (`include_dropped = true`): every feature card, whatever its release
//!   status and kind. Despite its name the flag lifts both filters at once.
//! - Notes whose lifecycle status is `downgraded` or `garbage` are always excluded by the
//!   SQL query itself, whatever the flag says.
//!
//! ## Ordering
//!
//! Ascending feature identifier, compared numerically on the digits of the identifier.

use anyhow::{Context, Result};

// Réexportation des types partagés pour compatibilité avec les appelants existants.
pub use gradatum_core::project_map::{
    DerivedExport, ExportOptions, FeatureEntry, project_map_feature_entries_derived_scoped,
};

/// Pure SQL logic: takes an already-open connection, which makes it testable against
/// `Connection::open_in_memory()`.
///
/// Queries the `project-map` section of `notes`, excluding the `downgraded` and `garbage`
/// lifecycle states, collects the body and title of each note, and delegates the
/// projection to [`gradatum_core::project_map::project_map_feature_entries`].
///
/// # Errors
///
/// - The SQL query fails.
/// - A row cannot be read.
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
        .context("preparing export-features query")?;

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
        .context("executing export-features query")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("reading export-features rows")?;

    // Délégation de la projection pure à gradatum-core (SSOT partagée avec le serveur).
    Ok(gradatum_core::project_map::project_map_feature_entries(
        &notes, opts,
    ))
}

/// Async wrapper: opens the SQLite index and delegates to [`export_features_from_conn`].
///
/// `root` is the Gradatum root directory, for example `/var/lib/gradatum`.
///
/// # Errors
///
/// - `index.db` cannot be found, meaning the server has never started on this root.
/// - The SQLite connection cannot be opened.
/// - The SQL query fails.
pub async fn export_features(
    root: &std::path::Path,
    vault: &str,
    opts: ExportOptions,
) -> Result<Vec<FeatureEntry>> {
    use gradatum_core::paths::vault_index_path;

    let db_path = vault_index_path(root);
    if !db_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the server must have started at least once",
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

/// Pure SQL logic for the **derived** export (make-before-break).
///
/// Same query and same connection contract as [`export_features_from_conn`], but each card's
/// `release` is **derived from its `[[track:]]` pointer** (via the structure index + `derive_release`)
/// instead of read from the stored `[[release:]]`. Runs alongside the stored path, never replaces
/// it — the two [`Vec<FeatureEntry>`] are meant to be compared before any hard switch.
///
/// Returns a [`DerivedExport`]: the derived entries plus a per-card diagnostic list for every card
/// whose release could not be derived and fell back to its stored value. The caller **must** surface
/// those diagnostics (the CLI logs them to stderr) — never a silent fallback.
///
/// # Errors
///
/// - The SQL query fails.
/// - A row cannot be read.
pub fn export_features_derived_from_conn(
    conn: &rusqlite::Connection,
    vault: &str,
    opts: ExportOptions,
) -> Result<DerivedExport> {
    // Requête identique à export_features_from_conn : section project-map, hors lifecycle.
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
        .context("preparing export-features (derived) query")?;

    let notes: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![vault], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .context("executing export-features (derived) query")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("reading export-features (derived) rows")?;

    // Projection dérivée (SSOT gradatum-core, partagée avec le serveur). `None` = tous projets.
    Ok(project_map_feature_entries_derived_scoped(
        &notes, opts, None,
    ))
}

/// Async wrapper for the **derived** export: opens the SQLite index and delegates to
/// [`export_features_derived_from_conn`].
///
/// `root` is the Gradatum root directory, for example `/var/lib/gradatum`.
///
/// # Errors
///
/// - `index.db` cannot be found, meaning the server has never started on this root.
/// - The SQLite connection cannot be opened.
/// - The SQL query fails.
pub async fn export_features_derived(
    root: &std::path::Path,
    vault: &str,
    opts: ExportOptions,
) -> Result<DerivedExport> {
    use gradatum_core::paths::vault_index_path;

    let db_path = vault_index_path(root);
    if !db_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the server must have started at least once",
            db_path.display()
        );
    }

    let db_path = db_path.clone();
    let vault = vault.to_string();

    tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open(&db_path)
            .with_context(|| format!("ouverture index.db : {}", db_path.display()))?;
        export_features_derived_from_conn(&conn, &vault, opts)
    })
    .await
    .context("spawn_blocking export_features_derived")?
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

    // ── Voie DÉRIVÉE (make-before-break, F-184 Phase 6) ─────────────────────────

    /// Derived path: a DONE feature card tracking an OBSOLETE internal ROADMAP derives `released`
    /// (rule 1), matching the stored value — no divergence, no diagnostic.
    #[test]
    fn derived_export_matches_stored_for_done_card_over_obsolete_roadmap() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);
        insert_note(
            &conn,
            "roadmap-070",
            "main",
            "project-map",
            "[[project:gradatum]] [[status:OBSOLETE]] [[kind:ROADMAP]] [[version:gradatum/0.7.0]] [[visibilite:interne]]",
            "ROADMAP 0.7.0",
            "live",
        );
        insert_note(
            &conn,
            "f-40",
            "main",
            "project-map",
            "[[feature:F-40]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] [[release:released]] [[version:gradatum/0.7.0]] [[track:gradatum/0.7.0]]",
            "F-40 livrée",
            "live",
        );

        let out = export_features_derived_from_conn(&conn, "main", ExportOptions::default())
            .expect("export dérivé");
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].feature, "F-40");
        assert_eq!(out.entries[0].release, "released");
        assert!(
            out.diagnostics.is_empty(),
            "aucune dérivation en échec : {:?}",
            out.diagnostics
        );
    }

    /// Regression: after the irreversible removal of stored `[[release:]]`/`[[version:]]`, a
    /// feature card carrying **only** its `[[track:]]` (no stored release nor version) plus a
    /// DONE ROADMAP must export 1 entry whose `release` AND `version` are derived from the track.
    /// Before the fix the derived path required the stored value as an anchor and returned 0.
    #[test]
    fn derived_export_projects_card_with_track_only_no_stored() {
        let conn = Connection::open_in_memory().expect("DB mémoire");
        create_schema(&conn);
        insert_note(
            &conn,
            "roadmap-210",
            "main",
            "project-map",
            "[[project:gradatum]] [[status:DONE]] [[kind:ROADMAP]] [[version:gradatum/2.1.0]] [[visibilite:public]]",
            "ROADMAP 2.1.0",
            "live",
        );
        insert_note(
            &conn,
            "f-80",
            "main",
            "project-map",
            // Forme post-retrait : pas de [[release:]] ni [[version:]], seulement [[track:]].
            "[[feature:F-80]] [[project:gradatum]] [[status:IN_PROGRESS]] [[kind:FEATURE]] [[track:gradatum/2.1.0]]",
            "F-80 sur 2.1.0",
            "live",
        );

        let out = export_features_derived_from_conn(&conn, "main", ExportOptions::default())
            .expect("export dérivé");
        assert_eq!(
            out.entries.len(),
            1,
            "la carte à track-seulement doit être reprojetée : {out:?}"
        );
        assert_eq!(out.entries[0].feature, "F-80");
        assert_eq!(
            out.entries[0].release, "released",
            "dérivé du track (ROADMAP DONE)"
        );
        assert_eq!(
            out.entries[0].version,
            Some("v2.1.0".to_string()),
            "version dérivée de l'identité de la structure pointée"
        );
        assert!(
            out.diagnostics.is_empty(),
            "dérivation réussie sans stocké : aucun diagnostic : {:?}",
            out.diagnostics
        );
    }
}
