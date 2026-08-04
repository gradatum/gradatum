//! `gradatum-admin project-map scope` — read-only summary view of a single project.
//!
//! Reads the SQLite index directly, with no HTTP call involved. It selects the
//! `project-map` cards belonging to one project — those carrying `[[project:<name>]]` —
//! and computes the per-status counters, the distinct versions and the current version.
//!
//! ## Layout
//!
//! - [`crate::project_map_scope::project_scope_from_conn`] holds the pure SQL logic and
//!   takes an open connection, so it can be tested against `Connection::open_in_memory()`.
//! - [`crate::project_map_scope::project_scope`] is the async wrapper used by the CLI,
//!   running the blocking work on a dedicated thread.
//!
//! ## Wikilink extraction
//!
//! No regular expressions: the body is scanned for `[[key:value]]` spans, and each span
//! is split on its first `:` to yield the `(key, value)` pair.
//!
//! ## Current version
//!
//! The highest version, in numeric version order, among the cards whose status is `DONE`.

use anyhow::{Context, Result};
use gradatum_core::paths::vault_index_path;

/// Summary of a single project's project-map cards.
#[derive(Debug, Default)]
pub struct ProjectScope {
    /// Project name, as carried by the `[[project:<name>]]` link.
    pub project: String,
    /// Current version: the highest version among the `DONE` cards.
    pub current_version: Option<String>,
    /// Number of cards in status `OPEN`.
    pub open_count: usize,
    /// Number of cards in status `IN_PROGRESS`.
    pub in_progress_count: usize,
    /// Number of cards in status `BLOCKED`.
    pub blocked_count: usize,
    /// Number of cards in status `DONE`.
    pub done_count: usize,
    /// Total number of active cards, excluding the `downgraded` and `garbage` states.
    pub total_count: usize,
    /// Distinct versions found on the cards, sorted from newest to oldest.
    pub versions: Vec<String>,
}

/// Extracts the `(key, value)` pairs of every `[[key:value]]` wikilink in a body.
///
/// The scan uses no regular expressions and is character-safe. Every pair found is
/// returned, in order of appearance.
///
/// Visible to the whole crate, though [`project_scope_from_conn`] is currently its only
/// caller: feature export goes through
/// [`gradatum_core::project_map::project_map_feature_entries`] instead.
pub(crate) fn extract_typed_links(body: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find("]]") {
            let target = &after_open[..end];
            if let Some((key, value)) = target.split_once(':') {
                pairs.push((key.to_string(), value.to_string()));
            }
            rest = &after_open[end + 2..];
        } else {
            break;
        }
    }
    pairs
}

/// Turns an `x.y.z` version into a numeric triple, so versions can be ordered
/// numerically rather than lexicographically.
///
/// Each component is parsed independently; a component that is missing or unparsable
/// counts as `0`, and any pre-release suffix such as `-rc.1` is dropped before parsing.
fn semver_tuple(v: &str) -> (u64, u64, u64) {
    let parts: Vec<&str> = v.split('.').collect();
    let n = |i: usize| -> u64 {
        parts
            .get(i)
            .and_then(|p| p.split('-').next()?.parse().ok())
            .unwrap_or(0)
    };
    (n(0), n(1), n(2))
}

/// Pure SQL logic: takes an already-open connection, which makes it testable against
/// `Connection::open_in_memory()`.
///
/// # Errors
///
/// - The SQL query fails.
/// - A row cannot be read.
pub fn project_scope_from_conn(
    conn: &rusqlite::Connection,
    vault: &str,
    project: &str,
) -> Result<ProjectScope> {
    // La recherche via LIKE '%[[project:<name>]]%' est robuste même si la valeur
    // contient des caractères spéciaux : les noms de projet sont validés [a-z0-9._-].
    let sql = "
        SELECT n.id, n.body_text, n.title, n.status
        FROM notes n
        WHERE n.vault_id = ?1
          AND n.section = 'project-map'
          AND n.status != 'downgraded'
          AND n.status != 'garbage'
          AND n.body_text LIKE '%[[project:' || ?2 || ']]%'
        ORDER BY n.created DESC
    ";

    let mut stmt = conn
        .prepare(sql)
        .context("preparing project-map scope query")?;

    // Collecte : (body_text, status)
    let rows: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![vault, project], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(3)?))
        })
        .context("executing project-map scope query")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("reading project-map scope rows")?;

    let mut scope = ProjectScope {
        project: project.to_string(),
        total_count: rows.len(),
        ..Default::default()
    };

    let mut done_versions: Vec<String> = Vec::new();
    let mut all_versions: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // Passe unique : extraction wikilinks typés (status + version) + comptage statuts.
    // Supprime la double boucle précédente (passe partielle + remise à zéro + passe complète).
    for (body, _) in &rows {
        let pairs = extract_typed_links(body);
        let mut note_status: Option<&str> = None;
        let mut note_version: Option<String> = None;

        for (key, value) in &pairs {
            match key.as_str() {
                "status" => note_status = Some(value.as_str()),
                "version" => {
                    // format: "gradatum/x.y.z" → extraire la partie version après "/"
                    if let Some((_, ver)) = value.split_once('/') {
                        note_version = Some(ver.to_string());
                    }
                }
                _ => {}
            }
        }

        // Comptage statuts depuis les wikilinks typés — source unique de vérité.
        match note_status {
            Some("OPEN") => scope.open_count += 1,
            Some("IN_PROGRESS") => scope.in_progress_count += 1,
            Some("BLOCKED") => scope.blocked_count += 1,
            Some("DONE") => scope.done_count += 1,
            _ => {}
        }

        if let Some(ver) = note_version {
            all_versions.insert(ver.clone());
            if note_status == Some("DONE") {
                done_versions.push(ver);
            }
        }
    }

    // Version courante = max SemVer parmi les cartes DONE
    scope.current_version = done_versions
        .iter()
        .max_by_key(|v| semver_tuple(v))
        .cloned();

    // Versions distinctes triées desc SemVer
    let mut versions: Vec<String> = all_versions.into_iter().collect();
    versions.sort_by_key(|v| std::cmp::Reverse(semver_tuple(v)));
    scope.versions = versions;

    Ok(scope)
}

/// Async version: opens the SQLite index and delegates to [`project_scope_from_conn`].
///
/// `root` is the Gradatum root directory, for example `/var/lib/gradatum`.
///
/// # Errors
///
/// - `index.db` cannot be found, meaning the server has never started on this root.
/// - The SQLite connection cannot be opened.
/// - The SQL query fails.
pub async fn project_scope(
    root: &std::path::Path,
    vault: &str,
    project: &str,
) -> Result<ProjectScope> {
    let db_path = vault_index_path(root);
    if !db_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the server must have started at least once",
            db_path.display()
        );
    }

    let db_path = db_path.clone();
    let vault = vault.to_string();
    let project = project.to_string();

    // spawn_blocking : rusqlite est synchrone, safe depuis un thread tokio current_thread.
    tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open(&db_path)
            .with_context(|| format!("ouverture index.db : {}", db_path.display()))?;
        project_scope_from_conn(&conn, &vault, &project)
    })
    .await
    .context("spawn_blocking project_scope")?
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    /// Corps d'une carte project-map de test avec les 4 wikilinks typés.
    fn card_body(project: &str, status: &str, version: &str) -> String {
        format!(
            "[[project:{project}]] [[status:{status}]] [[kind:FEATURE]] [[version:{project}/{version}]]\n\nItem de test."
        )
    }

    fn create_test_db_with_notes() -> Connection {
        let conn = Connection::open_in_memory().expect("DB mémoire pour test");
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
        conn
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

    #[test]
    fn scope_returns_correct_counts() {
        let conn = create_test_db_with_notes();
        insert_note(
            &conn,
            "n1",
            "main",
            "project-map",
            &card_body("gradatum", "OPEN", "0.5.2"),
            "Item 1",
            "live",
        );
        insert_note(
            &conn,
            "n2",
            "main",
            "project-map",
            &card_body("gradatum", "IN_PROGRESS", "0.5.2"),
            "Item 2",
            "live",
        );
        insert_note(
            &conn,
            "n3",
            "main",
            "project-map",
            &card_body("gradatum", "DONE", "0.5.2"),
            "Item 3",
            "live",
        );
        insert_note(
            &conn,
            "n4",
            "main",
            "project-map",
            &card_body("gradatum", "BLOCKED", "0.5.2"),
            "Item 4",
            "live",
        );

        let scope = project_scope_from_conn(&conn, "main", "gradatum").expect("scope");
        assert_eq!(scope.total_count, 4);
        assert_eq!(scope.open_count, 1, "open");
        assert_eq!(scope.in_progress_count, 1, "in_progress");
        assert_eq!(scope.blocked_count, 1, "blocked");
        assert_eq!(scope.done_count, 1, "done");
    }

    #[test]
    fn scope_current_version_is_max_done_version() {
        let conn = create_test_db_with_notes();
        insert_note(
            &conn,
            "n1",
            "main",
            "project-map",
            &card_body("gradatum", "DONE", "0.4.6"),
            "Old DONE",
            "live",
        );
        insert_note(
            &conn,
            "n2",
            "main",
            "project-map",
            &card_body("gradatum", "DONE", "0.5.2"),
            "Newest DONE",
            "live",
        );
        insert_note(
            &conn,
            "n3",
            "main",
            "project-map",
            &card_body("gradatum", "OPEN", "0.6.0"),
            "Open future",
            "live",
        );

        let scope = project_scope_from_conn(&conn, "main", "gradatum").expect("scope");
        assert_eq!(
            scope.current_version.as_deref(),
            Some("0.5.2"),
            "version courante doit être le max des DONE"
        );
    }

    #[test]
    fn scope_excludes_downgraded_and_garbage() {
        let conn = create_test_db_with_notes();
        insert_note(
            &conn,
            "n1",
            "main",
            "project-map",
            &card_body("gradatum", "DONE", "0.5.2"),
            "Visible",
            "live",
        );
        insert_note(
            &conn,
            "n2",
            "main",
            "project-map",
            &card_body("gradatum", "OPEN", "0.5.2"),
            "Downgraded",
            "downgraded",
        );
        insert_note(
            &conn,
            "n3",
            "main",
            "project-map",
            &card_body("gradatum", "DONE", "0.5.2"),
            "Garbage",
            "garbage",
        );

        let scope = project_scope_from_conn(&conn, "main", "gradatum").expect("scope");
        assert_eq!(scope.total_count, 1, "downgraded et garbage exclus");
    }

    #[test]
    fn scope_versions_sorted_desc() {
        let conn = create_test_db_with_notes();
        insert_note(
            &conn,
            "n1",
            "main",
            "project-map",
            &card_body("gradatum", "DONE", "0.4.5"),
            "v0.4.5",
            "live",
        );
        insert_note(
            &conn,
            "n2",
            "main",
            "project-map",
            &card_body("gradatum", "DONE", "0.5.2"),
            "v0.5.2",
            "live",
        );
        insert_note(
            &conn,
            "n3",
            "main",
            "project-map",
            &card_body("gradatum", "OPEN", "0.4.6"),
            "v0.4.6",
            "live",
        );

        let scope = project_scope_from_conn(&conn, "main", "gradatum").expect("scope");
        // Versions triées desc : 0.5.2, 0.4.6, 0.4.5
        assert_eq!(scope.versions, vec!["0.5.2", "0.4.6", "0.4.5"]);
    }

    #[test]
    fn scope_empty_project_returns_zero() {
        let conn = create_test_db_with_notes();
        // Aucune note pour ce projet
        let scope = project_scope_from_conn(&conn, "main", "inexistant").expect("scope");
        assert_eq!(scope.total_count, 0);
        assert_eq!(scope.open_count, 0);
        assert!(scope.current_version.is_none());
        assert!(scope.versions.is_empty());
    }

    #[test]
    fn scope_only_counts_cards_of_requested_project() {
        let conn = create_test_db_with_notes();
        insert_note(
            &conn,
            "n1",
            "main",
            "project-map",
            &card_body("gradatum", "DONE", "0.5.2"),
            "gradatum card",
            "live",
        );
        insert_note(
            &conn,
            "n2",
            "main",
            "project-map",
            &card_body("other-project", "OPEN", "1.0.0"),
            "other card",
            "live",
        );

        let scope = project_scope_from_conn(&conn, "main", "gradatum").expect("scope");
        assert_eq!(
            scope.total_count, 1,
            "seules les cartes de 'gradatum' comptent"
        );
    }
}
