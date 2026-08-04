//! End-to-end tests for the project-map TODO generator.
//!
//! Exercises the full **graph path** of `render_from_index`: an in-memory SQLite
//! index is seeded with 2 project-map cards (notes + typed edges in `note_links`),
//! then `TODO.md` is generated and verified to reflect the graph only
//! (no semantic search — the graph is the authoritative source of work-status).
//!
//! These tests cover the integration path that pure rendering unit tests do not:
//! `backlinks` + `trace_lineage` + `get_titles_sections` on a real DB.

use gradatum_admin::project_map_render::{GENERATED_MARKER, render_from_index};
use gradatum_core::identity::NoteId;
use gradatum_index::SqliteIndex;
use ulid::Ulid;

/// Insère une carte project-map (note + titre) et ses arêtes typées réservées.
async fn seed_card(
    index: &SqliteIndex,
    id: &str,
    title: &str,
    status_node: &str,
    version_node: Option<&str>,
) {
    index
        .seed_note_with_fts(id, "project-map", &format!("# {title}\n\ncorps"))
        .await
        .expect("seed note carte");
    index
        .upsert_note_title(
            "main",
            &NoteId(Ulid::from_string(id).expect("ulid valide")),
            title,
        )
        .await
        .expect("titre carte");

    // Arêtes typées : project, status, version (nœuds réservés, dst TEXT libre).
    index
        .upsert_link("main", id, "project:gradatum")
        .await
        .expect("arête project");
    index
        .upsert_link("main", id, status_node)
        .await
        .expect("arête status");
    if let Some(v) = version_node {
        index
            .upsert_link("main", id, v)
            .await
            .expect("arête version");
    }
}

/// 2 cartes ouvertes (1 versionnée IN_PROGRESS, 1 non attribuée OPEN) + 1 DONE
/// → le `TODO.md` généré liste les 2 ouvertes groupées, exclut la DONE, et porte
/// le marqueur généré.
#[tokio::test]
async fn render_from_graph_produces_correct_todo() {
    let index = SqliteIndex::open_in_memory()
        .await
        .expect("index in-memory");

    // ULID valides (26 chars Crockford).
    let card_open_versioned = "01KVBTMYNK4XXZJAKWMTB4AM01";
    let card_open_unassigned = "01KVBTMYNK4XXZJAKWMTB4AM02";
    let card_done = "01KVBTMYNK4XXZJAKWMTB4AM03";

    seed_card(
        &index,
        card_open_versioned,
        "Feature versionnée",
        "status:IN_PROGRESS",
        Some("version:gradatum/0.6.1"),
    )
    .await;
    seed_card(
        &index,
        card_open_unassigned,
        "Tâche non attribuée",
        "status:OPEN",
        None,
    )
    .await;
    seed_card(
        &index,
        card_done,
        "Feature terminée",
        "status:DONE",
        Some("version:gradatum/0.6.0"),
    )
    .await;

    let md = render_from_index(&index, "main", "gradatum")
        .await
        .expect("render");

    // Marqueur généré en tête.
    assert!(md.starts_with(GENERATED_MARKER), "marqueur généré:\n{md}");
    // Les 2 cartes ouvertes sont listées.
    assert!(
        md.contains("Feature versionnée"),
        "carte IN_PROGRESS listée:\n{md}"
    );
    assert!(
        md.contains("Tâche non attribuée"),
        "carte OPEN listée:\n{md}"
    );
    // La carte DONE est exclue (pas de data-loss mais pas dans le backlog).
    assert!(!md.contains("Feature terminée"), "carte DONE exclue:\n{md}");
    // Groupement par version + section non attribuée.
    assert!(md.contains("## gradatum/0.6.1"), "groupe versionné:\n{md}");
    assert!(md.contains("- [IN_PROGRESS] Feature versionnée"));
    assert!(md.contains("## Unassigned"), "groupe non attribué:\n{md}");
    assert!(md.contains("- [OPEN] Tâche non attribuée"));
}

/// Un projet sans aucune carte ouverte produit un TODO vide marqué (pas d'erreur).
#[tokio::test]
async fn render_empty_project_is_marked_empty() {
    let index = SqliteIndex::open_in_memory()
        .await
        .expect("index in-memory");
    let md = render_from_index(&index, "main", "gradatum")
        .await
        .expect("render vide");
    assert!(md.starts_with(GENERATED_MARKER));
    assert!(md.contains("_No open item._"), "TODO vide marqué:\n{md}");
}

/// Une carte d'un AUTRE projet n'apparaît pas dans le TODO du projet demandé
/// (le filtrage est porté par l'arête `project:` du graphe, pas la sémantique).
#[tokio::test]
async fn render_isolates_by_project_node() {
    let index = SqliteIndex::open_in_memory()
        .await
        .expect("index in-memory");

    // Carte du projet "example-project" (arête project:example-project), OPEN.
    let card = "01KVBTMYNK4XXZJAKWMTB4AM04";
    index
        .seed_note_with_fts(card, "project-map", "# Carte example-project\n\ncorps")
        .await
        .unwrap();
    index
        .upsert_note_title(
            "main",
            &NoteId(Ulid::from_string(card).unwrap()),
            "Carte example-project",
        )
        .await
        .unwrap();
    index
        .upsert_link("main", card, "project:example-project")
        .await
        .unwrap();
    index
        .upsert_link("main", card, "status:OPEN")
        .await
        .unwrap();

    let md = render_from_index(&index, "main", "gradatum")
        .await
        .expect("render gradatum");
    assert!(
        !md.contains("Carte example-project"),
        "carte d'un autre projet ne doit pas fuiter:\n{md}"
    );
    assert!(md.contains("_No open item._"));
}
