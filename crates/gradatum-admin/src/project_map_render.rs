//! `gradatum-admin project-map render <project>` — generates `TODO.md` (PULL view).
//!
//! `TODO.md` is a **derived view** of the project-map: it is never edited by hand
//! but regenerated on demand from the **wikilink/work-status graph**. Semantic
//! search is intentionally excluded — a `DONE` card downgraded from semantic
//! scoring would disappear from the backlog, causing data loss; the graph is
//! deterministic and authoritative.
//!
//! ## PULL generation
//!
//! There is no push-on-write: the view is produced by this explicit command, not
//! coupled to the write path.
//!
//! ## Algorithm, reading the `note_links` graph
//!
//! 1. `backlinks("project:<p>")` yields the cards of the project.
//! 2. For each card, `trace_lineage(card).children` yields its outgoing typed edges,
//!    such as `status:X` and `version:project/version`.
//! 3. Keep only the cards whose status is `OPEN`, `IN_PROGRESS` or `BLOCKED`.
//! 4. `get_titles_sections` fetches all titles in one batch, avoiding a query per card.
//! 5. [`crate::project_map_render::render_todo_markdown`] groups the result by version
//!    and prepends the generated-file marker.

use std::collections::BTreeMap;

use anyhow::Context;
use gradatum_core::paths::vault_index_path;
use gradatum_core::project_map::{ProjectMapLink, StatusKind};
use gradatum_index::SqliteIndex;

/// Header marker written at the top of every generated `TODO.md` file.
///
/// Signals that the file is machine-generated and must not be edited by hand.
pub const GENERATED_MARKER: &str = "<!-- generated project-map, do not edit by hand -->";

/// Work statuses that appear in the generated `TODO.md` backlog.
///
/// Only `OPEN`, `IN_PROGRESS`, and `BLOCKED` cards are actionable.
/// `DONE`, `OBSOLETE`, and `BRAINSTORMING` are excluded from the backlog view.
const OPEN_STATUSES: [StatusKind; 3] = [
    StatusKind::Open,
    StatusKind::InProgress,
    StatusKind::Blocked,
];

/// One open work item of a project, projected for the `TODO.md` rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    /// Human-readable card title taken from its H1, falling back to its ULID.
    pub title: String,
    /// Work status. Only `OPEN`, `IN_PROGRESS`, and `BLOCKED` appear in the view.
    pub status: StatusKind,
    /// Namespaced target version, shaped as `project/x.y.z`, or `None` when unassigned.
    pub version: Option<String>,
}

/// Extracts the work status and the target version of a card from its outgoing edges.
///
/// `children` holds the raw destination identifiers returned by `trace_lineage`, mixing
/// reserved nodes such as `status:DONE` or `version:gradatum/0.6.1` with ordinary
/// dependencies. Each one is re-parsed through
/// [`gradatum_core::project_map::parse_link`] — the single parser shared across the
/// codebase — to single out the status and the version.
///
/// Returns `None` when no status edge is present.
#[must_use]
pub fn work_status_from_children(children: &[String]) -> Option<(StatusKind, Option<String>)> {
    let mut status: Option<StatusKind> = None;
    let mut version: Option<String> = None;

    for child in children {
        match gradatum_core::project_map::parse_link(child) {
            Ok(ProjectMapLink::Status(s)) => status = Some(s),
            Ok(ProjectMapLink::Version {
                project,
                version: v,
            }) => {
                version = Some(format!("{project}/{v}"));
            }
            _ => {}
        }
    }

    status.map(|s| (s, version))
}

/// Renders the `TODO.md` of a project from its open work items (pure function).
///
/// Filters to open statuses (`OPEN`/`IN_PROGRESS`/`BLOCKED`), groups by version — items
/// with no version land in an `Unassigned` section rendered last — sorts deterministically,
/// and prepends the generated-file marker. This is a **pure function** (no I/O),
/// testable in isolation.
///
/// Sort order is deterministic: versions in ascending lexicographic order via
/// `BTreeMap`; items sorted by title within each group.
#[must_use]
pub fn render_todo_markdown(project: &str, items: &[WorkItem]) -> String {
    // Groupe : version (Some) ou None → items ouverts uniquement.
    let mut by_version: BTreeMap<Option<String>, Vec<&WorkItem>> = BTreeMap::new();
    for item in items {
        if OPEN_STATUSES.contains(&item.status) {
            by_version
                .entry(item.version.clone())
                .or_default()
                .push(item);
        }
    }

    let mut out = String::new();
    out.push_str(GENERATED_MARKER);
    out.push('\n');
    out.push_str(&format!("# TODO — {project}\n\n"));

    if by_version.is_empty() {
        out.push_str("_No open item._\n");
        return out;
    }

    // Ordre déterministe : versions attribuées croissantes d'abord, groupe
    // « Non attribué » (None) en dernier. (L'ordre dérivé de Option place None
    // avant Some — on le force donc explicitement ici.)
    let mut groups: Vec<(&Option<String>, &Vec<&WorkItem>)> = by_version.iter().collect();
    groups.sort_by(|a, b| match (a.0, b.0) {
        (Some(va), Some(vb)) => va.cmp(vb),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    for (version, group) in groups {
        let heading = match version {
            Some(v) => format!("## {v}"),
            None => "## Unassigned".to_string(),
        };
        out.push_str(&heading);
        out.push('\n');

        // Items triés par titre (déterminisme).
        let mut sorted: Vec<&&WorkItem> = group.iter().collect();
        sorted.sort_by(|a, b| a.title.cmp(&b.title));
        for item in sorted {
            out.push_str(&format!("- [{}] {}\n", item.status.as_wire(), item.title));
        }
        out.push('\n');
    }

    out
}

/// Generates the `TODO.md` of a project by querying the index graph.
///
/// Opens the SQLite index in read mode, reads the graph (via `backlinks`,
/// `trace_lineage`, and `get_titles_sections`), and returns the rendered
/// Markdown. **No semantic search** — the graph is the authoritative source.
///
/// # Errors
///
/// Returns an error if the index cannot be found or if a graph query fails.
pub async fn render_project_map(
    root: &std::path::Path,
    vault_id: &str,
    project: &str,
) -> anyhow::Result<String> {
    let db_path = vault_index_path(root);
    if !db_path.exists() {
        anyhow::bail!(
            "index.db not found: {} — the server must have started at least once",
            db_path.display()
        );
    }
    let index = SqliteIndex::open(&db_path)
        .await
        .context("opening index.db for project-map render")?;

    render_from_index(&index, vault_id, project).await
}

/// Core graph logic of [`render_project_map`] operating on an already-open index.
///
/// Separated for testability: integration tests can inject an in-memory
/// `SqliteIndex` without going through a filesystem `root` path.
///
/// # Errors
///
/// Returns an error if a graph query fails.
pub async fn render_from_index(
    index: &SqliteIndex,
    vault_id: &str,
    project: &str,
) -> anyhow::Result<String> {
    let project_node = format!("project:{project}");
    let cards = index
        .backlinks(vault_id, &project_node)
        .await
        .context("backlinks project node")?;

    // Restreindre au périmètre faisant autorité — la MÊME population que
    // `project-map scope` : cartes vivantes (hors `downgraded`/`garbage`) de la
    // section `project-map`. Le graphe `note_links` n'est PAS purgé quand une note
    // est downgradée : un doublon downgradé conserve ses arêtes `status:`/`version:`
    // et serait sinon rendu comme carte ouverte (F-214). Ce filtrage porte sur la
    // population source des cartes, en amont de l'extraction de statut et du rendu —
    // ce n'est pas un filtre posé à la sortie du rendu.
    //
    // Deux lectures batch (anti-N+1) sur l'ensemble des cartes candidates :
    // - `get_statuses`         → statut de cycle de vie (`live`/`downgraded`/`garbage`)
    // - `get_titles_sections`  → titre + section
    let lifecycles = index
        .get_statuses(vault_id, &cards)
        .await
        .context("get_statuses (filtre cycle de vie)")?;
    let titles = index
        .get_titles_sections(vault_id, &cards)
        .await
        .context("get_titles_sections")?;

    let project_map_section = gradatum_core::section::Section::ProjectMap.as_str();

    let mut items: Vec<WorkItem> = Vec::new();
    // Statut/version par carte retenue (résolution du titre déjà en main, anti-N+1).
    let mut staged: Vec<(String, StatusKind, Option<String>)> = Vec::new();

    for card_id in &cards {
        // Garde cycle de vie : exclut `downgraded`/`garbage` (miroir du SQL de scope).
        // Une carte absente de la table (arête orpheline) est traitée comme morte.
        match lifecycles.get(card_id).map(String::as_str) {
            Some("downgraded" | "garbage") | None => continue,
            Some(_) => {}
        }
        // Garde de section : seules les cartes `project-map` composent la vue TODO
        // (miroir de scope). Écarte le bruit inter-sections (p. ex. notes
        // `architecture` portant `[[project:…]]` + `[[status:…]]`).
        if titles.get(card_id).map(|(_, s)| s.as_str()) != Some(project_map_section) {
            continue;
        }

        let lineage = index
            .trace_lineage(vault_id, card_id)
            .await
            .context("trace_lineage carte")?;
        if let Some((status, version)) = work_status_from_children(&lineage.children)
            && OPEN_STATUSES.contains(&status)
        {
            staged.push((card_id.clone(), status, version));
        }
    }

    for (card_id, status, version) in staged {
        // Titre H1 si présent, sinon repli sur l'ULID (carte sans H1).
        let title = titles
            .get(&card_id)
            .and_then(|(t, _)| t.clone())
            .unwrap_or_else(|| card_id.clone());
        items.push(WorkItem {
            title,
            status,
            version,
        });
    }

    Ok(render_todo_markdown(project, &items))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, status: StatusKind, version: Option<&str>) -> WorkItem {
        WorkItem {
            title: title.to_string(),
            status,
            version: version.map(str::to_string),
        }
    }

    #[test]
    fn marker_and_heading_present() {
        let md = render_todo_markdown("gradatum", &[]);
        assert!(md.starts_with(GENERATED_MARKER), "marqueur généré en tête");
        assert!(md.contains("# TODO — gradatum"));
        assert!(md.contains("_No open item._"));
    }

    #[test]
    fn done_items_are_excluded() {
        let items = [
            item("Carte finie", StatusKind::Done, Some("gradatum/0.6.0")),
            item("Carte ouverte", StatusKind::Open, Some("gradatum/0.6.1")),
        ];
        let md = render_todo_markdown("gradatum", &items);
        assert!(!md.contains("Carte finie"), "DONE exclu du TODO");
        assert!(md.contains("Carte ouverte"), "OPEN listé");
    }

    #[test]
    fn obsolete_and_brainstorming_excluded() {
        let items = [
            item("Abandonnée", StatusKind::Obsolete, None),
            item("Idée amont", StatusKind::Brainstorming, None),
            item("À faire", StatusKind::Open, None),
        ];
        let md = render_todo_markdown("gradatum", &items);
        assert!(!md.contains("Abandonnée"));
        assert!(!md.contains("Idée amont"));
        assert!(md.contains("À faire"));
    }

    /// Cas du plan : 3 notes (1 DONE, 2 OPEN dont 1 versionnée) → seules les 2
    /// OPEN listées, groupées correctement, avec marqueur.
    #[test]
    fn plan_fixture_groups_by_version() {
        let items = [
            item("Feature A", StatusKind::Done, Some("gradatum/0.6.0")),
            item("Feature B", StatusKind::InProgress, Some("gradatum/0.6.1")),
            item("Tâche C", StatusKind::Open, None),
        ];
        let md = render_todo_markdown("gradatum", &items);

        assert!(md.starts_with(GENERATED_MARKER));
        // DONE exclu.
        assert!(!md.contains("Feature A"));
        // Groupe versionné.
        assert!(md.contains("## gradatum/0.6.1"));
        assert!(md.contains("- [IN_PROGRESS] Feature B"));
        // Groupe non attribué en dernier.
        assert!(md.contains("## Unassigned"));
        assert!(md.contains("- [OPEN] Tâche C"));
        // Ordre : la section versionnée précède « Non attribué ».
        let pos_ver = md.find("## gradatum/0.6.1").unwrap();
        let pos_none = md.find("## Unassigned").unwrap();
        assert!(pos_ver < pos_none, "versions attribuées avant non-attribué");
    }

    #[test]
    fn work_status_from_children_extracts_status_and_version() {
        let children = vec![
            "project:gradatum".to_string(),
            "status:BLOCKED".to_string(),
            "version:gradatum/0.6.2".to_string(),
            "decisions:01KVBTMYNK4XXZJAKWMTB4AM9K".to_string(),
        ];
        let (status, version) = work_status_from_children(&children).unwrap();
        assert_eq!(status, StatusKind::Blocked);
        assert_eq!(version.as_deref(), Some("gradatum/0.6.2"));
    }

    #[test]
    fn work_status_from_children_none_when_no_status() {
        let children = vec!["project:gradatum".to_string(), "kind:FIX".to_string()];
        assert_eq!(work_status_from_children(&children), None);
    }

    #[test]
    fn work_status_from_children_no_version_is_none() {
        let children = vec!["status:OPEN".to_string()];
        let (status, version) = work_status_from_children(&children).unwrap();
        assert_eq!(status, StatusKind::Open);
        assert_eq!(version, None);
    }
}
