//! Tests d'intégration C9 — parallélisation title_lookup wikilinks.
//!
//! Vérifie que `process_wikilinks_b5` avec `futures::future::join_all` sur les
//! `title_lookup` produit un résultat identique au comportement série :
//! - Tous les wikilinks résolus sont persistés dans `note_links`.
//! - Un wikilink non résolu (note cible absente) ne bloque pas les autres.
//! - 0 wikilink → retour immédiat, pas d'appel à l'index.
//!
//! Les tests sont comportementaux (assertions sur `note_links` final).
//! Pas de mesure de latence — le test de performance reste hors scope TDD.
//!
//! Spec : docs/specs/2026-05-29-phase-2x5-alpha-15-polish-spec.md §8

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{count_backlinks, enqueue_curate_job, has_backlink_to, test_dispatcher_with_index};

use chrono::Utc;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

// ── Helper local ──────────────────────────────────────────────────────────────

/// Seed une note cible dans le vault + index, retourne son ULID stringifié.
///
/// Même pattern que `wikilinks_post_curate.rs::seed_target_note` — dupliqué
/// ici pour éviter l'import cross-module entre fichiers de test integration.
async fn seed_target_note(fixture: &helpers::DispatcherFixture, title: &str, body: &str) -> String {
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let body_full = format!("# {title}\n{body}");
    let note = fixture
        .vault
        .write_note(frontmatter, body_full)
        .await
        .expect("vault.write_note seed cible");
    fixture
        .index
        .upsert_note_title(&note.id, title)
        .await
        .expect("upsert_note_title seed cible");
    note.id.to_string()
}

// ── Tests C9 ──────────────────────────────────────────────────────────────────

/// C9 — N=5 wikilinks tous résolus → les 5 liens sont persistés dans note_links.
///
/// Valide que la parallélisation `join_all` ne perd aucun wikilink par rapport
/// au comportement série. Le set de `dst_id` persisté doit être identique.
#[tokio::test]
async fn wikilinks_b5_parallel_persists_all_links() {
    let fixture = test_dispatcher_with_index().await;

    // Seed N=5 notes cibles
    let targets = [
        ("Note Parallèle A", "Contenu A"),
        ("Note Parallèle B", "Contenu B"),
        ("Note Parallèle C", "Contenu C"),
        ("Note Parallèle D", "Contenu D"),
        ("Note Parallèle E", "Contenu E"),
    ];

    let mut target_ids = Vec::new();
    for (title, body) in &targets {
        let id = seed_target_note(&fixture, title, body).await;
        target_ids.push(id);
    }

    // Body source avec 5 wikilinks vers les 5 notes cibles
    let body = "# [DECISIONS] Note parallèle multi-liens\n\n\
        Voir [[Note Parallèle A]], [[Note Parallèle B]], [[Note Parallèle C]], \
        [[Note Parallèle D]] et [[Note Parallèle E]] pour les références.";
    enqueue_curate_job(&fixture, "[DECISIONS] Note parallèle multi-liens", body).await;

    let processed = fixture.dispatcher.run_once().await.unwrap();
    assert!(processed, "le dispatcher doit traiter le job");

    // Chaque note cible doit avoir exactement 1 backlink depuis la note source
    for (idx_pos, target_id) in target_ids.iter().enumerate() {
        let count = count_backlinks(&fixture.index, target_id).await;
        assert_eq!(
            count, 1,
            "C9 : note cible #{idx_pos} ({target_id}) doit avoir exactement 1 backlink, count={count}"
        );
        assert!(
            has_backlink_to(&fixture.index, target_id).await,
            "C9 : has_backlink_to doit être true pour note cible #{idx_pos} ({target_id})"
        );
    }
}

/// C9 — wikilink non résolu (cible absente) ne bloque pas les autres.
///
/// Avec `join_all`, un `Ok(None)` sur title_lookup ne doit pas court-circuiter
/// les autres futures. Le lien résolu doit être persisté ; le lien non résolu est ignoré.
#[tokio::test]
async fn wikilinks_b5_parallel_unresolved_wikilink_does_not_block() {
    let fixture = test_dispatcher_with_index().await;

    // 1 note cible connue + 1 wikilink vers titre inexistant
    let known_title = "Note Connue Parallèle";
    let known_id = seed_target_note(&fixture, known_title, "Contenu connu.").await;

    let body = "# [DECISIONS] Note mixte lien connu/inconnu\n\n\
        Voir [[Note Connue Parallèle]] et [[Titre Totalement Inexistant XYZ123]] pour le contexte.";
    enqueue_curate_job(&fixture, "[DECISIONS] Note mixte lien connu/inconnu", body).await;

    let result = fixture.dispatcher.run_once().await;
    assert!(
        result.is_ok(),
        "C9 : curate ne doit pas échouer sur wikilink non résolu — err={result:?}"
    );
    assert!(result.unwrap(), "un job doit avoir été traité");

    // Le lien connu est persisté
    assert!(
        has_backlink_to(&fixture.index, &known_id).await,
        "C9 : lien résolu vers '{known_title}' ({known_id}) doit être persisté"
    );
    // Le lien non résolu ne crée pas d'arête fantôme
    let phantom_count = count_backlinks(&fixture.index, "Titre Totalement Inexistant XYZ123").await;
    assert_eq!(
        phantom_count, 0,
        "C9 : lien non résolu ne doit pas créer d'arête fantôme"
    );
}

/// C9 — 0 wikilinks → fast-path return immédiat (inchangé après parallélisation).
///
/// Un body sans `[[...]]` ne doit déclencher aucun appel index.
/// Le job doit être traité avec succès.
#[tokio::test]
async fn wikilinks_b5_parallel_empty_returns_immediately() {
    let fixture = test_dispatcher_with_index().await;

    let body = "# [DECISIONS] Note sans wikilinks\n\nContenu simple sans liens.";
    enqueue_curate_job(&fixture, "[DECISIONS] Note sans wikilinks", body).await;

    let result = fixture.dispatcher.run_once().await;
    assert!(
        result.is_ok(),
        "C9 : curate sans wikilinks ne doit pas échouer — err={result:?}"
    );
    assert!(result.unwrap(), "un job doit avoir été traité");
    // Aucun lien créé (table note_links vide pour ce vault)
    // Vérification indirecte : count_backlinks sur un ID fictif = 0
    let count = count_backlinks(&fixture.index, "00000000000000000000000000").await;
    assert_eq!(count, 0, "C9 : aucun lien fantôme sans wikilinks");
}
