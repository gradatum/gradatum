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
//! ## Algorithme (lecture du graphe `note_links`)
//!
//! 1. `backlinks("project:<p>")` → cartes du projet.
//! 2. pour chaque carte : `trace_lineage(carte).children` → ses arêtes sortantes
//!    typées (`status:X`, `version:p/v`).
//! 3. ne garder que les cartes au statut ∈ {OPEN, IN_PROGRESS, BLOCKED}.
//! 4. `get_titles_sections` (batch, anti-N+1) → titres.
//! 5. `render_todo_markdown` → markdown groupé par version + marqueur généré.

use std::collections::BTreeMap;

use anyhow::Context;
use gradatum_core::paths::vault_index_path;
use gradatum_core::project_map::{ProjectMapLink, StatusKind};
use gradatum_index::SqliteIndex;

/// Header marker written at the top of every generated `TODO.md` file.
///
/// Signals that the file is machine-generated and must not be edited by hand.
pub const GENERATED_MARKER: &str = "<!-- généré project-map, ne pas éditer à la main -->";

/// Work statuses that appear in the generated `TODO.md` backlog.
///
/// Only `OPEN`, `IN_PROGRESS`, and `BLOCKED` cards are actionable.
/// `DONE`, `OBSOLETE`, and `BRAINSTORMING` are excluded from the backlog view.
const OPEN_STATUSES: [StatusKind; 3] = [
    StatusKind::Open,
    StatusKind::InProgress,
    StatusKind::Blocked,
];

/// Une unité de travail ouverte d'un projet, projetée pour le rendu `TODO.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    /// Titre humain de la carte (H1), ou son ULID en repli.
    pub title: String,
    /// Work status. Only `OPEN`, `IN_PROGRESS`, and `BLOCKED` appear in the view.
    pub status: StatusKind,
    /// Version cible namespacée `projet/x.y.z`, ou `None` si non attribuée.
    pub version: Option<String>,
}

/// Extrait le work-status et la version d'une carte depuis ses arêtes sortantes.
///
/// `children` = `dst_note_id` bruts (`trace_lineage(carte).children`) : nœuds
/// réservés (`status:DONE`, `version:gradatum/0.6.1`) et dépendances/annexes.
/// On reparse via [`gradatum_core::project_map::parse_link`] (SSOT) pour isoler
/// le statut et la version. Retourne `(status, version)` si un statut est trouvé.
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
/// Filters to open statuses (`OPEN`/`IN_PROGRESS`/`BLOCKED`), groups by version
/// (`None` = "Non attribué" section, rendered last), sorts deterministically, and
/// prepends the generated-file marker. This is a **pure function** (no I/O) —
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
        out.push_str("_Aucun item ouvert._\n");
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
            None => "## Non attribué".to_string(),
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
/// Retourne une erreur si l'index est introuvable ou si une requête graphe échoue.
pub async fn render_project_map(
    root: &std::path::Path,
    vault_id: &str,
    project: &str,
) -> anyhow::Result<String> {
    let db_path = vault_index_path(root);
    if !db_path.exists() {
        anyhow::bail!(
            "index.db introuvable : {} — le server doit avoir démarré au moins une fois",
            db_path.display()
        );
    }
    let index = SqliteIndex::open(&db_path)
        .await
        .context("ouverture index.db pour project-map render")?;

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

    let mut items: Vec<WorkItem> = Vec::new();
    let mut open_ids: Vec<String> = Vec::new();
    // Statut/version par carte ouverte (avant résolution du titre, anti-N+1).
    let mut staged: Vec<(String, StatusKind, Option<String>)> = Vec::new();

    for card_id in &cards {
        let lineage = index
            .trace_lineage(vault_id, card_id)
            .await
            .context("trace_lineage carte")?;
        if let Some((status, version)) = work_status_from_children(&lineage.children)
            && OPEN_STATUSES.contains(&status)
        {
            open_ids.push(card_id.clone());
            staged.push((card_id.clone(), status, version));
        }
    }

    // Résolution des titres en batch (anti-N+1).
    let titles = index
        .get_titles_sections(vault_id, &open_ids)
        .await
        .context("get_titles_sections")?;

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
        assert!(md.contains("_Aucun item ouvert._"));
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
        assert!(md.contains("## Non attribué"));
        assert!(md.contains("- [OPEN] Tâche C"));
        // Ordre : la section versionnée précède « Non attribué ».
        let pos_ver = md.find("## gradatum/0.6.1").unwrap();
        let pos_none = md.find("## Non attribué").unwrap();
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
