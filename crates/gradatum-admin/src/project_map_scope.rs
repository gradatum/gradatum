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
use gradatum_core::project_map::StatusKind;

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
    /// Number of cards in status `OBSOLETE`.
    pub obsolete_count: usize,
    /// Number of cards in status `BRAINSTORMING`.
    pub brainstorming_count: usize,
    /// Number of cards carrying no recognised `[[status:…]]` wikilink — no status
    /// edge at all, an unknown value, or a future [`StatusKind`] variant. These are
    /// the cards that a per-status breakdown would otherwise silently drop.
    pub unaccounted_count: usize,
    /// Total number of active cards, excluding the `downgraded` and `garbage` states.
    pub total_count: usize,
    /// Distinct versions found on the cards, sorted from newest to oldest.
    pub versions: Vec<String>,
}

impl ProjectScope {
    /// Sum of every per-status counter, including [`ProjectScope::unaccounted_count`].
    ///
    /// Each card is classified into exactly one bucket, so this sum is expected to
    /// equal [`ProjectScope::total_count`]. [`ProjectScope::reconciliation_gap`]
    /// surfaces any residual, which must never be left for the reader to deduce.
    #[must_use]
    pub fn status_sum(&self) -> usize {
        self.open_count
            + self.in_progress_count
            + self.blocked_count
            + self.done_count
            + self.obsolete_count
            + self.brainstorming_count
            + self.unaccounted_count
    }

    /// Signed gap between the total and the sum of every status counter.
    ///
    /// Zero by construction (every card lands in exactly one bucket). Kept as an
    /// explicit, displayed quantity so that any future drift is named in the output
    /// rather than inferred from two non-comparable numbers.
    #[must_use]
    pub fn reconciliation_gap(&self) -> i64 {
        i64::try_from(self.total_count).unwrap_or(i64::MAX)
            - i64::try_from(self.status_sum()).unwrap_or(i64::MAX)
    }
}

/// Exit code of `project-map scope` when the reconciliation holds — the total equals
/// the sum of every per-status counter.
pub const EXIT_SCOPE_RECONCILED: i32 = 0;

/// Exit code of `project-map scope` when the total and the sum of the per-status
/// counters disagree.
///
/// Deliberately `2`, not `1`. Inside `gradatum-admin` the two are not interchangeable:
///
/// - `1` is what `main() -> anyhow::Result<()>` renders through `Termination` when the
///   binary could **not** do its work — no `index.db`, a failed query, a bad argument.
///   `jobs get` / `jobs cancel` exit `1` on the same grounds.
/// - `2` is what `drift-scan` already reserves for "the work was done, the verdict is
///   negative". This command joins that convention rather than inventing one.
///
/// Keeping them apart is what lets a caller distinguish a discrepancy that was measured
/// from a failure to look for it. Collapsing both onto one non-zero code would make an
/// unreconciled count indistinguishable from a broken invocation — and a caller that
/// treats every non-zero code as "could not measure" would stop reading the very line
/// that names the discrepancy.
pub const EXIT_SCOPE_UNRECONCILED: i32 = 2;

/// Verdict of `project-map scope`, rendered as a process exit code.
///
/// [`EXIT_SCOPE_RECONCILED`] when [`ProjectScope::reconciliation_gap`] is zero,
/// [`EXIT_SCOPE_UNRECONCILED`] otherwise — **in either direction**. A total larger than
/// the sum means cards were dropped from the breakdown; a sum larger than the total
/// means cards were counted twice. Both are a broken count, and neither may render
/// success: the criterion is that a gap of this shape makes a check *fail*, not that it
/// gets printed.
#[must_use]
pub fn scope_exit_code(scope: &ProjectScope) -> i32 {
    if scope.reconciliation_gap() == 0 {
        EXIT_SCOPE_RECONCILED
    } else {
        EXIT_SCOPE_UNRECONCILED
    }
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
        // La classification passe par `StatusKind::from_wire` (vocabulaire faisant
        // autorité), et non par une liste locale : un statut hors des quatre historiques
        // (OBSOLETE, BRAINSTORMING) est compté ; l'absence de statut, une valeur inconnue
        // ou un futur variant #[non_exhaustive] tombent dans `unaccounted_count` — jamais
        // perdus en silence.
        match note_status.and_then(StatusKind::from_wire) {
            Some(StatusKind::Open) => scope.open_count += 1,
            Some(StatusKind::InProgress) => scope.in_progress_count += 1,
            Some(StatusKind::Blocked) => scope.blocked_count += 1,
            Some(StatusKind::Done) => scope.done_count += 1,
            Some(StatusKind::Obsolete) => scope.obsolete_count += 1,
            Some(StatusKind::Brainstorming) => scope.brainstorming_count += 1,
            _ => scope.unaccounted_count += 1,
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

    /// F-207 : un jeu contenant un statut hors des quatre historiques (OBSOLETE,
    /// BRAINSTORMING) et une carte sans wikilink de statut doit produire une somme
    /// de compteurs strictement égale au total — aucune carte perdue en silence.
    #[test]
    fn scope_all_statuses_sum_to_total() {
        let conn = create_test_db_with_notes();
        insert_note(
            &conn,
            "n1",
            "main",
            "project-map",
            &card_body("gradatum", "OPEN", "0.5.2"),
            "Ouverte",
            "live",
        );
        insert_note(
            &conn,
            "n2",
            "main",
            "project-map",
            &card_body("gradatum", "DONE", "0.5.2"),
            "Finie",
            "live",
        );
        insert_note(
            &conn,
            "n3",
            "main",
            "project-map",
            &card_body("gradatum", "OBSOLETE", "0.5.2"),
            "Obsolète",
            "live",
        );
        insert_note(
            &conn,
            "n4",
            "main",
            "project-map",
            &card_body("gradatum", "BRAINSTORMING", "0.5.2"),
            "Idée amont",
            "live",
        );
        // Carte project-map sans wikilink `[[status:…]]` → bucket « non classé ».
        insert_note(
            &conn,
            "n5",
            "main",
            "project-map",
            "[[project:gradatum]] [[kind:FEATURE]]\n\nSans statut.",
            "Sans statut",
            "live",
        );

        let scope = project_scope_from_conn(&conn, "main", "gradatum").expect("scope");
        assert_eq!(scope.total_count, 5);
        assert_eq!(scope.open_count, 1, "OPEN");
        assert_eq!(scope.done_count, 1, "DONE");
        assert_eq!(
            scope.obsolete_count, 1,
            "OBSOLETE compté (hors 4 historiques)"
        );
        assert_eq!(
            scope.brainstorming_count, 1,
            "BRAINSTORMING compté (hors 4 historiques)"
        );
        assert_eq!(
            scope.unaccounted_count, 1,
            "carte sans wikilink status → non classé"
        );
        // Invariant central F-207 : somme de tous les compteurs == total.
        assert_eq!(
            scope.status_sum(),
            scope.total_count,
            "somme des statuts doit égaler le total"
        );
        assert_eq!(scope.reconciliation_gap(), 0, "écart réconcilié");
    }

    // ─── F-207 critère 3 — l'écart rend un VERDICT, pas seulement une ligne ──

    /// Écart nul ⇒ code 0. C'est le cas nominal, et le seul que le consommateur
    /// doit lire comme « la mesure a été faite et elle tient ».
    #[test]
    fn exit_code_is_zero_when_reconciled() {
        let scope = ProjectScope {
            total_count: 4,
            open_count: 2,
            done_count: 2,
            ..Default::default()
        };
        assert_eq!(scope.reconciliation_gap(), 0, "prémisse du test");
        assert_eq!(scope_exit_code(&scope), EXIT_SCOPE_RECONCILED);
    }

    /// Total > somme (des cartes ont disparu de la ventilation) ⇒ code non nul.
    ///
    /// C'est LE cas que le critère 3 vise : avant ce lot, cette situation imprimait
    /// « NON RÉCONCILIÉ » puis rendait 0 — aucun contrôle ne pouvait s'y accrocher.
    #[test]
    fn exit_code_is_unreconciled_when_total_exceeds_sum() {
        let scope = ProjectScope {
            total_count: 366,
            open_count: 100,
            done_count: 260,
            ..Default::default()
        };
        assert_eq!(scope.reconciliation_gap(), 6, "prémisse du test");
        assert_eq!(scope_exit_code(&scope), EXIT_SCOPE_UNRECONCILED);
    }

    /// Somme > total (des cartes comptées deux fois) ⇒ code non nul lui aussi.
    ///
    /// L'écart est signé : ne garder que le sens positif laisserait passer un double
    /// comptage, qui est exactement le même défaut vu de l'autre côté.
    #[test]
    fn exit_code_is_unreconciled_when_sum_exceeds_total() {
        let scope = ProjectScope {
            total_count: 3,
            open_count: 2,
            done_count: 2,
            ..Default::default()
        };
        assert_eq!(scope.reconciliation_gap(), -1, "prémisse du test");
        assert_eq!(scope_exit_code(&scope), EXIT_SCOPE_UNRECONCILED);
    }

    /// Le code de verdict négatif doit rester DISCERNABLE du code d'échec d'exécution.
    ///
    /// `main() -> anyhow::Result<()>` rend `1` via `Termination` quand le binaire n'a
    /// pas pu faire son travail (index.db absent, requête en échec). Si le verdict
    /// « non réconcilié » prenait la même valeur, un consommateur ne pourrait plus
    /// distinguer « j'ai mesuré un écart » de « je n'ai pas pu mesurer » — et
    /// basculerait en dégradé au lieu de lever l'anomalie.
    #[test]
    fn unreconciled_code_differs_from_execution_failure_code() {
        /// Code rendu par `Termination` pour un `Err` remonté de `main`.
        const EXIT_EXECUTION_FAILURE: i32 = 1;
        assert_ne!(EXIT_SCOPE_UNRECONCILED, EXIT_SCOPE_RECONCILED);
        assert_ne!(EXIT_SCOPE_UNRECONCILED, EXIT_EXECUTION_FAILURE);
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
