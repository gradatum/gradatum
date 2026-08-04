//! Parité backend : préservation du trust dynamique au re-upsert (v0.4.4 P1-1).
//!
//! Invariant critique introduit en v0.4.4 (distillation F-22) :
//! un trust dynamique posé par `set_note_trust` (ex. 0.96 calculé par la
//! distillation) NE DOIT PAS être écrasé par le trust statique dérivé de la
//! provenance lors d'une réécriture de la note **à provenance inchangée**.
//!
//! Sans cette garantie, chaque re-curate d'une note distillée réinitialiserait
//! son trust à la valeur statique TRUST_SCORES — perte de l'information de
//! confiance calculée.

mod common;

use common::{make_index, make_note_with_id, minimal_frontmatter};
use gradatum_core::identity::NoteId;

#[tokio::test]
async fn set_and_get_trust_roundtrips() {
    let idx = make_index().await;
    let note = make_note_with_id(
        NoteId::new(),
        minimal_frontmatter("main"),
        "trust roundtrip",
    );
    idx.write_note(&note).await.expect("write");

    let affected = idx
        .set_note_trust("main", &note.id, 0.87)
        .await
        .expect("set_note_trust");
    assert_eq!(
        affected,
        1,
        "1 ligne mise à jour ({})",
        common::backend_label()
    );

    let trust = idx
        .get_trust("main", &note.id)
        .await
        .expect("get_trust")
        .expect("trust présent");
    assert!(
        (trust - 0.87).abs() < 1e-6,
        "trust relu = {trust} (attendu 0.87) ({})",
        common::backend_label()
    );
}

#[tokio::test]
async fn dynamic_trust_preserved_on_reupsert_same_provenance() {
    let idx = make_index().await;
    let id = NoteId::new();
    // provenance reste None (inchangée) entre les deux écritures.
    let note = make_note_with_id(id, minimal_frontmatter("main"), "note distillée");
    idx.write_note(&note).await.expect("write initial");

    // La distillation pose un trust dynamique calculé.
    idx.set_note_trust("main", &id, 0.96)
        .await
        .expect("set_note_trust dynamique");
    assert!(
        (idx.get_trust("main", &id)
            .await
            .expect("get")
            .expect("trust")
            - 0.96)
            .abs()
            < 1e-6,
        "trust dynamique posé"
    );

    // Re-upsert de la MÊME note (provenance inchangée) — ex. re-curate / reindex.
    let note_again = make_note_with_id(id, minimal_frontmatter("main"), "note distillée v2");
    idx.write_note(&note_again).await.expect("re-upsert");

    let after = idx
        .get_trust("main", &id)
        .await
        .expect("get after")
        .expect("trust after");
    assert!(
        (after - 0.96).abs() < 1e-6,
        "trust dynamique 0.96 préservé au re-upsert (provenance inchangée), obtenu {after} ({})",
        common::backend_label()
    );
}

#[tokio::test]
async fn set_note_trust_absent_note_affects_zero_rows() {
    let idx = make_index().await;
    let affected = idx
        .set_note_trust("main", &NoteId::new(), 0.5)
        .await
        .expect("set_note_trust sur note absente");
    assert_eq!(
        affected,
        0,
        "note absente → 0 ligne ({})",
        common::backend_label()
    );
}
