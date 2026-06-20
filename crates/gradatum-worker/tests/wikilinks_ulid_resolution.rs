//! Tests e2e B5 — résolution wikilinks par ULID (ULID-first).
//!
//! Valide le fix du bug note_links vide en LIVE :
//! les wikilinks `[[section:ULID]]` doivent être résolus via `id_lookup`
//! (court-circuit du lookup H1), produisant des arêtes dans `note_links`.
//!
//! ## Cas couverts
//!
//! 1. `[[section:ULID]]` vers une note existante → 1 arête (résolution ULID-first)
//! 2. `[[decisions:ULID-inexistant]]` → 0 arête (pas de lien dangling)
//! 3. `[[Titre Humain H1]]` → 1 arête (fallback title_lookup — rétrocompat)
//!
//! Utilise [`helpers::DispatcherFixture`] et [`helpers::MockInternalClient`] qui
//! délèguent à `SqliteIndex::id_lookup` en local.

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{count_backlinks, enqueue_curate_job, has_backlink_to, test_dispatcher_with_index};

/// Cas 1 : `[[section:ULID]]` vers une note existante et live → 1 arête dans note_links.
///
/// Simule exactement le format écrit par le vault : `[[decisions:01KV...]]`.
/// Après curate, `note_links` doit contenir l'arête src→dst.
#[tokio::test]
async fn wikilink_section_ulid_format_produces_link() {
    let fixture = test_dispatcher_with_index().await;

    // Seed une note cible — on récupère son ULID.
    let target_id = seed_live_note(&fixture, "Note Cible ULID-First", "Contenu cible.").await;

    // Corps avec wikilink au format `[[section:ULID]]` — format réel vault.
    // Le `section` devant `:` peut être n'importe quoi ; seul l'ULID compte.
    let body = format!(
        "# Source ULID-First\n\nVoir [[decisions:{target_id}]] pour la décision originale."
    );
    enqueue_curate_job(&fixture, "[DECISIONS] Source ULID-First", &body).await;

    let processed = fixture.dispatcher.run_once().await.unwrap();
    assert!(
        processed,
        "le dispatcher doit signaler qu'un job a été traité"
    );

    // La note cible doit avoir exactement 1 backlink (arête ULID-first résolue).
    let count = count_backlinks(&fixture.index, &target_id).await;
    assert_eq!(
        count, 1,
        "ULID-first : exactement 1 arête doit pointer vers la note cible (count={count})"
    );
    assert!(
        has_backlink_to(&fixture.index, &target_id).await,
        "has_backlink_to doit confirmer l'arête vers {target_id}"
    );
}

/// Cas 2 : `[[decisions:ULID-inexistant]]` → 0 arête (pas de lien dangling).
///
/// L'ULID est syntaxiquement valide (26 chars Crockford) mais absent du vault.
/// `id_lookup` doit retourner `None` → lien ignoré, `note_links` reste vide.
#[tokio::test]
async fn wikilink_section_ulid_nonexistent_produces_no_link() {
    let fixture = test_dispatcher_with_index().await;

    // ULID valide mais absent du vault (jamais inséré)
    let ghost_ulid = "01JZZZZZZZZZZZZZZZZZZZZZZ0";
    let body = format!("# Source ULID Ghost\n\nVoir [[decisions:{ghost_ulid}]] — note fantôme.");
    enqueue_curate_job(&fixture, "[DECISIONS] Source ULID Ghost", &body).await;

    let result = fixture.dispatcher.run_once().await;
    assert!(
        result.is_ok(),
        "curate ne doit pas échouer sur ULID inexistant — err={result:?}"
    );
    assert!(result.unwrap(), "un job doit avoir été traité");

    // Aucune arête ne doit pointer vers l'ULID fantôme.
    let count = count_backlinks(&fixture.index, ghost_ulid).await;
    assert_eq!(
        count, 0,
        "ULID inexistant ne doit produire aucune arête (count={count})"
    );
}

/// Cas 3 : `[[Titre Humain H1]]` → 1 arête (fallback title_lookup — rétrocompat).
///
/// Garantit que les wikilinks anciens format (titre libre, non-ULID) continuent
/// à fonctionner via `title_lookup`.
#[tokio::test]
async fn wikilink_human_title_fallback_still_works() {
    let fixture = test_dispatcher_with_index().await;

    let title = "Note Cible Titre Humain Alpha99";
    let target_id = seed_live_note(&fixture, title, "Contenu cible humain.").await;

    // Wikilink au format titre humain — ne contient pas d'ULID.
    let body = format!("# Source Titre Humain\n\nVoir [[{title}]] — résolution via title_lookup.");
    enqueue_curate_job(&fixture, "[DECISIONS] Source Titre Humain", &body).await;

    fixture.dispatcher.run_once().await.unwrap();

    // L'arête doit exister (fallback title_lookup préservé).
    assert!(
        has_backlink_to(&fixture.index, &target_id).await,
        "title_lookup fallback doit toujours produire une arête vers {target_id}"
    );
}

// ── Helpers locaux ────────────────────────────────────────────────────────────

/// Seed une note live via le vault, retourne son ULID string.
///
/// La note est insérée avec `status=Live` et son titre est upsert dans l'index
/// pour que `title_lookup` puisse aussi la trouver (rétrocompat Cas 3).
async fn seed_live_note(fixture: &helpers::DispatcherFixture, title: &str, body: &str) -> String {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Decisions,
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
        .expect("vault.write_note seed note");
    // Upsert du titre pour le fallback title_lookup (rétrocompat).
    fixture
        .index
        .upsert_note_title(&note.id, title)
        .await
        .expect("upsert_note_title seed note");
    note.id.to_string()
}
