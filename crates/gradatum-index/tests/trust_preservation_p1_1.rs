//! P1-1 (audit lot B) — préservation du trust dynamique au re-upsert.
//!
//! `upsert_note` dérive `trust` statiquement depuis `provenance` (TRUST_SCORES).
//! Un trust DYNAMIQUE posé par `set_note_trust` (F-22 distillation) ne doit PAS être
//! écrasé par le re-upsert si la provenance est inchangée. Si la provenance change,
//! le trust statique est recalculé.

mod common;
use common::make_note;

use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// Construit une note avec une provenance explicite (le helper commun met `None`).
fn note_with_provenance(provenance: Option<&str>, body: &str) -> gradatum_core::note::Note {
    let mut note = make_note("main", Section::Reference, NoteStatus::Live, body);
    note.frontmatter.provenance = provenance.map(|s| s.to_string());
    // Recalcul du content_hash après mutation du frontmatter (invariant d'unicité).
    note.content_hash =
        gradatum_core::identity::ContentHash::compute(&note.frontmatter, &note.body.markdown);
    note
}

/// Provenance inchangée → un trust dynamique posé par set_note_trust survit au re-upsert.
#[tokio::test]
async fn dynamic_trust_preserved_when_provenance_unchanged() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Note distillée : provenance "distilled" → trust statique 0.60 à l'upsert initial.
    let note = note_with_provenance(Some("distilled"), "synthèse distillée");
    idx.upsert_note(&note).await.expect("upsert initial");
    let id = note.id;

    // Le trust statique distilled = 0.60 (TRUST_SCORES).
    let t0 = idx
        .get_trust("main", &id)
        .await
        .expect("get_trust")
        .expect("trust non NULL");
    assert!(
        (t0 - 0.60).abs() < 1e-4,
        "trust statique distilled attendu 0.60, got {t0}"
    );

    // F-22 : pose un trust DYNAMIQUE 0.42 (gradatum_distill::compute_distill_trust).
    idx.set_note_trust("main", &id, 0.42)
        .await
        .expect("set_note_trust");
    let t1 = idx
        .get_trust("main", &id)
        .await
        .expect("get_trust")
        .expect("trust");
    assert!(
        (t1 - 0.42).abs() < 1e-4,
        "trust dynamique attendu 0.42, got {t1}"
    );

    // Re-upsert de la MÊME note (provenance inchangée) — ex. ré-indexation, embed cascade.
    idx.upsert_note(&note).await.expect("re-upsert");
    let t2 = idx
        .get_trust("main", &id)
        .await
        .expect("get_trust")
        .expect("trust");
    assert!(
        (t2 - 0.42).abs() < 1e-4,
        "P1-1 : trust dynamique DOIT survivre au re-upsert (provenance inchangée), got {t2}"
    );
}

/// Provenance changée → le trust statique est recalculé (le dynamique est légitimement remplacé).
#[tokio::test]
async fn static_trust_recomputed_when_provenance_changes() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    let note = note_with_provenance(Some("distilled"), "note");
    idx.upsert_note(&note).await.expect("upsert initial");
    let id = note.id;

    // Pose un trust dynamique 0.42.
    idx.set_note_trust("main", &id, 0.42)
        .await
        .expect("set_note_trust");

    // Re-upsert avec une provenance DIFFÉRENTE (human-decision → trust statique 0.95).
    let mut note2 = note.clone();
    note2.frontmatter.provenance = Some("human-decision".to_string());
    note2.content_hash =
        gradatum_core::identity::ContentHash::compute(&note2.frontmatter, &note2.body.markdown);
    idx.upsert_note(&note2)
        .await
        .expect("re-upsert provenance changée");

    let t = idx
        .get_trust("main", &id)
        .await
        .expect("get_trust")
        .expect("trust");
    assert!(
        (t - 0.95).abs() < 1e-4,
        "P1-1 : provenance changée → trust statique recalculé 0.95, got {t}"
    );
}

/// Provenance NULL→NULL inchangée → trust dynamique préservé (cas NULL géré par IS NOT).
#[tokio::test]
async fn dynamic_trust_preserved_when_provenance_null_unchanged() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Provenance None → trust statique défaut 0.50 à l'upsert.
    let note = note_with_provenance(None, "note sans provenance");
    idx.upsert_note(&note).await.expect("upsert initial");
    let id = note.id;

    idx.set_note_trust("main", &id, 0.33)
        .await
        .expect("set_note_trust");

    // Re-upsert avec provenance toujours None — IS NOT doit considérer NULL==NULL inchangé.
    idx.upsert_note(&note).await.expect("re-upsert");
    let t = idx
        .get_trust("main", &id)
        .await
        .expect("get_trust")
        .expect("trust");
    assert!(
        (t - 0.33).abs() < 1e-4,
        "P1-1 : NULL→NULL inchangé → trust dynamique préservé, got {t}"
    );
}
