//! Tests d'intégration — classificateur heuristique offline.
//!
//! 4 patterns testés :
//! 1. Keyword `decision`/`architecture` → confiance > 0.7 + `Live`
//! 2. Corps très court (< 50 chars) → `PendingReview` confidence 0.50
//! 3. Ambiguïté sans keywords → confiance ≤ 0.65
//! 4. Signal d'engagement wikilink `[[...]]` → boost confiance vs baseline

mod common;

use common::build_note_with_body;
use gradatum_chat::{Chat, ChatBackend, CuratorContext, Heuristic};
use gradatum_core::status::NoteStatus;

#[tokio::test]
async fn clear_admit_decision_keyword() {
    let h = Heuristic::new();
    let note = build_note_with_body(
        "This is a clear architecture decision for the gradatum indexing pipeline. \
         We chose OpenDAL over a custom fs abstraction for portability reasons. \
         The tradeoffs are well understood and documented in §2.1.",
    );
    let v = h
        .classify_curator(&note, &CuratorContext::default())
        .await
        .unwrap();
    assert!(
        v.confidence > 0.7,
        "keyword 'architecture' devrait donner confidence > 0.7, obtenu: {}",
        v.confidence
    );
    assert_eq!(
        v.proposed_status,
        NoteStatus::Live,
        "une note avec keyword architecture devrait être proposée Live"
    );
    assert_eq!(v.backend, ChatBackend::Heuristic);
}

#[tokio::test]
async fn clear_reject_short_body() {
    let h = Heuristic::new();
    // Body < 50 chars → confiance 0.50, PendingReview
    let note = build_note_with_body("trop court");
    let v = h
        .classify_curator(&note, &CuratorContext::default())
        .await
        .unwrap();
    assert_eq!(
        v.confidence, 0.50,
        "corps trop court devrait donner exactement 0.50"
    );
    assert_eq!(
        v.proposed_status,
        NoteStatus::PendingReview,
        "corps trop court devrait proposer PendingReview"
    );
    assert_eq!(v.backend, ChatBackend::Heuristic);
}

#[tokio::test]
async fn ambiguous_low_confidence() {
    let h = Heuristic::new();
    // Corps sans keywords ni wikilinks, suffisamment long pour dépasser le seuil court
    // mais sans signal sémantique fort → confiance ≤ 0.65
    let note = build_note_with_body(
        "Ceci est une note générique sans mots-clés particuliers. \
         Elle contient suffisamment de texte pour dépasser le seuil de longueur minimale \
         mais ne comporte aucun signal sémantique fort ni de wikilink ni de tags.",
    );
    let v = h
        .classify_curator(&note, &CuratorContext::default())
        .await
        .unwrap();
    assert!(
        v.confidence <= 0.65,
        "note sans signal sémantique devrait avoir confidence ≤ 0.65, obtenu: {}",
        v.confidence
    );
    assert_eq!(v.backend, ChatBackend::Heuristic);
}

#[tokio::test]
async fn engagement_signal_wikilinks() {
    let h = Heuristic::new();
    // Corps avec wikilinks mais sans keywords → boost par rapport à la note sans wikilink
    let note_with_wikilinks = build_note_with_body(
        "Cette note fait référence à [[gradatum-core]] et à [[gradatum-index]] pour illustrer \
         les connexions entre composants du système. Elle est suffisamment longue \
         pour passer le seuil et contient deux wikilinks Obsidian signalant l'engagement.",
    );
    let note_without_wikilinks = build_note_with_body(
        "Cette note fait référence à gradatum-core et à gradatum-index pour illustrer \
         les connexions entre composants du système. Elle est suffisamment longue \
         pour passer le seuil et ne contient aucun wikilink Obsidian.",
    );

    let v_with = h
        .classify_curator(&note_with_wikilinks, &CuratorContext::default())
        .await
        .unwrap();
    let v_without = h
        .classify_curator(&note_without_wikilinks, &CuratorContext::default())
        .await
        .unwrap();

    assert!(
        v_with.confidence > v_without.confidence,
        "wikilinks devraient booster la confiance: {} vs {}",
        v_with.confidence,
        v_without.confidence
    );
    assert_eq!(v_with.backend, ChatBackend::Heuristic);
}
