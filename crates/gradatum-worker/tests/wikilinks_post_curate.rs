//! Tests d'intégration B5 — wikilinks persistés après un job curate.
//!
//! Exercent le moteur ACTIF `handle_curate` (via `helpers::process_curate`).
//! Après la persistance de la note (branches Admitted/Pending), les wikilinks
//! `[[...]]` du body sont :
//! 1. Extraits via `gradatum_curator::wikilinks::extract_wikilinks(body)`
//! 2. Résolus via `id_lookup`/`title_lookup` (filtre `status='live'`)
//! 3. Empaquetés dans `PersistCuratedRequest.links` → `persist_curated` fait `upsert_link`
//!
//! Couvre 3 cas :
//! 1. `curate_admitted_upserts_wikilinks_into_note_links` — chemin Admitted
//! 2. `curate_with_unresolved_wikilink_does_not_fail` — non-fatal sur résolution KO
//! 3. `curate_pending_outcome_also_upserts_wikilinks` — chemin Pending

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{count_backlinks, has_backlink_to, process_curate, test_curate_fixture};

/// Test 1 : un job curate (Admitted) avec wikilink résolu doit persister le lien
/// dans `note_links`.
///
/// Setup : seed une note "Note Cible" via vault.write_note (titre H1 + upsert_note_title).
/// Le body curate contient `[[Note Cible]]`. Après `handle_curate`, `note_links` doit
/// contenir un lien src=<nouvelle note> → dst=<id Note Cible>.
#[tokio::test]
async fn curate_admitted_upserts_wikilinks_into_note_links() {
    let fixture = test_curate_fixture().await;

    // Seed la note cible via vault.write_note (fichier .md + index SQLite + upsert_note_title)
    let target_title = "Note Cible Alpha13";
    let target_id = seed_target_note(&fixture, target_title, "Contenu de référence cible.").await;

    // Préfixe `[DECISIONS]` → heuristique routing donne Admitted direct (confidence ≥ 0.8).
    // Body contient `[[Note Cible Alpha13]]` — wikilink à résoudre.
    let body = format!(
        "# Note avec lien décision\n\nVoir [[{target_title}]] pour le contexte de cette décision."
    );
    process_curate(&fixture, "[DECISIONS] Note avec lien décision", &body)
        .await
        .expect("handle_curate doit réussir");

    // La note cible doit avoir au moins un backlink (src = la note curate-ée).
    let count = count_backlinks(&fixture.index, &target_id).await;
    assert_eq!(
        count, 1,
        "B5 : exactement 1 wikilink doit pointer vers la note cible. count={count}"
    );
    assert!(
        has_backlink_to(&fixture.index, &target_id).await,
        "B5 : has_backlink_to doit retourner true vers Note Cible Alpha13"
    );
}

/// Test 2 : un wikilink vers une note inexistante ne doit PAS faire échouer le curate.
///
/// La note est admise et persistée. Le wikilink non résolu est ignoré (log debug,
/// pas d'erreur fatale). `note_links` reste vide.
#[tokio::test]
async fn curate_with_unresolved_wikilink_does_not_fail() {
    let fixture = test_curate_fixture().await;

    let body = "# Note orpheline décision\n\nLien vers [[Note Inexistante XYZ]] non résolu.";
    let result = process_curate(&fixture, "[DECISIONS] Note orpheline décision", body).await;
    assert!(
        result.is_ok(),
        "curate ne doit pas échouer sur wikilink non résolu — err={result:?}"
    );

    // Aucune note "Inexistante" en base → pas de lien créé (ni vers Inexistante,
    // ni vers une autre note arbitraire).
    let inexistent = has_backlink_to(&fixture.index, "Note Inexistante XYZ").await;
    assert!(!inexistent, "lien non résolu ne doit pas être persisté");
}

/// Test 3 (L-P0-1) : le branchage B5 s'applique aussi sur la branche
/// `CurateOutcome::Pending`.
///
/// Mécanisme déclencheur Pending : titre sans préfixe + body court → confidence
/// heuristique < 0.8 + `llm_review_enabled=false` (default) → Pending direct.
#[tokio::test]
async fn curate_pending_outcome_also_upserts_wikilinks() {
    let fixture = test_curate_fixture().await;

    let target_title = "Cible Pending Alpha13";
    let target_id = seed_target_note(&fixture, target_title, "Contenu cible pending.").await;

    // Titre simple sans préfixe explicite → heuristique confidence basse → Pending.
    let body =
        format!("## Brouillon\n\nVoir [[{target_title}]] — note brouillon en attente de revue.");
    process_curate(&fixture, "Brouillon en attente", &body)
        .await
        .expect("handle_curate doit réussir");

    // Le lien doit être persisté même pour un Pending (parité Admitted).
    assert!(
        has_backlink_to(&fixture.index, &target_id).await,
        "B5 doit s'appliquer aussi sur CurateOutcome::Pending — backlinks vers {target_id} attendu"
    );
}

// ── Helpers locaux ────────────────────────────────────────────────────────────

/// Seed une note via `vault.write_note` puis upsert le `title` colonne pour la
/// résolution `title_lookup`. Retourne l'ULID stringifié.
async fn seed_target_note(fixture: &helpers::CurateFixture, title: &str, body: &str) -> String {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

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
        .upsert_note_title(note.frontmatter.vault_id.as_str(), &note.id, title)
        .await
        .expect("upsert_note_title seed cible");
    note.id.to_string()
}
