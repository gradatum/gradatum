//! Test : fast path heuristique — confiance élevée ne déclenche pas le LLM.

mod common;
use common::build_note_with_body;

use gradatum_chat::{ChatBackend, CuratorContext, Heuristic, Noop};
use gradatum_core::config::CuratorConfig;
use gradatum_curator::Curator;
use std::sync::Arc;

/// Un corps riche contenant le keyword "decision" + wikilink → confiance > 0.7.
/// Le LLM (`Noop`) n'est pas consulté — `backend_used = Heuristic`.
#[tokio::test]
async fn fast_path_high_confidence_skips_llm() {
    let body = "This is a clear architecture decision about [[wikilink]] design \
                with thorough rationale and proper context for the team to understand \
                the trade-offs involved.";
    let note = build_note_with_body(body);
    let cfg = CuratorConfig {
        llm_review_enabled: Some(true),
        confidence_threshold: Some(0.7),
        ..Default::default()
    };
    // Noop retourne confidence=0.0 — si appelé, le test échouerait sur backend_used
    let curator: Curator<Noop> = Curator::new(Heuristic::new(), Some(Arc::new(Noop)), cfg);
    let decision = curator.decide(&note, &CuratorContext::default()).await;

    assert!(
        !decision.fallback_applied,
        "fast path ne doit pas marquer fallback_applied"
    );
    assert_eq!(
        decision.backend_used,
        ChatBackend::Heuristic,
        "fast path : backend doit rester Heuristic (pas de LLM)"
    );
    assert!(
        decision.confidence > 0.7,
        "confiance attendue > 0.7 pour ce body riche, obtenu {:.2}",
        decision.confidence
    );
}
