//! Test : LLM désactivé — confiance faible → PendingReview sans LLM, sans fallback.

mod common;
use common::build_note_with_body;

use gradatum_chat::{CuratorContext, Heuristic, Noop};
use gradatum_core::config::CuratorConfig;
use gradatum_core::status::NoteStatus;
use gradatum_curator::Curator;

/// Body court → heuristic 0.50 → llm_review_enabled=false → PendingReview.
/// `fallback_applied` doit être `false` : ce n'est pas un fallback, c'est le comportement
/// attendu quand la revue LLM est explicitement désactivée.
#[tokio::test]
async fn llm_review_disabled_low_conf_returns_pending_review() {
    let note = build_note_with_body("short");
    let cfg = CuratorConfig {
        llm_review_enabled: Some(false),
        ..Default::default()
    };
    // Pas de LLM fourni — le None est cohérent avec llm_review_enabled=false
    let curator: Curator<Noop> = Curator::new(Heuristic::new(), None, cfg);
    let decision = curator.decide(&note, &CuratorContext::default()).await;

    assert_eq!(
        decision.final_status,
        NoteStatus::PendingReview,
        "LLM désactivé + confiance faible → PendingReview"
    );
    assert!(
        !decision.fallback_applied,
        "LLM désactivé = comportement nominal, pas un fallback"
    );
}
