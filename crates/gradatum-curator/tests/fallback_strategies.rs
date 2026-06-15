//! Tests : 3 stratégies de fallback quand le LLM est indisponible.

mod common;
use common::build_note_with_body;

use async_trait::async_trait;
use gradatum_chat::{Chat, ChatBackend, ChatError, CuratorContext, CuratorVerdict, Heuristic};
use gradatum_core::config::CuratorConfig;
use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;
use gradatum_curator::Curator;
use std::sync::Arc;

/// Backend mock qui échoue toujours avec `Timeout`.
struct FailingLlm;

#[async_trait]
impl Chat for FailingLlm {
    async fn classify_curator(
        &self,
        _note: &Note,
        _ctx: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError> {
        Err(ChatError::Timeout)
    }

    fn backend_kind(&self) -> ChatBackend {
        ChatBackend::Http
    }
}

/// Construit un curator avec `FailingLlm` et la stratégie de fallback spécifiée.
fn curator_with_fallback(fallback_str: &str) -> Curator<FailingLlm> {
    let cfg = CuratorConfig {
        llm_review_enabled: Some(true),
        confidence_threshold: Some(0.7),
        llm_review_fallback: Some(fallback_str.into()),
        ..Default::default()
    };
    Curator::new(Heuristic::new(), Some(Arc::new(FailingLlm)), cfg)
}

/// Body court → heuristique 0.50 → LLM timeout → "pending-review-fallback" → PendingReview.
#[tokio::test]
async fn fallback_pending_review_default() {
    let note = build_note_with_body("ambiguous");
    let curator = curator_with_fallback("pending-review-fallback");
    let decision = curator.decide(&note, &CuratorContext::default()).await;

    assert_eq!(decision.final_status, NoteStatus::PendingReview);
    assert!(decision.fallback_applied, "fallback doit être marqué");
}

/// "reject" (strict) : LLM timeout → final = Garbage.
#[tokio::test]
async fn fallback_reject_strict() {
    let note = build_note_with_body("ambiguous");
    let curator = curator_with_fallback("reject");
    let decision = curator.decide(&note, &CuratorContext::default()).await;

    assert_eq!(
        decision.final_status,
        NoteStatus::Garbage,
        "stratégie reject → final doit être Garbage"
    );
    assert!(decision.fallback_applied);
    assert!(
        (decision.confidence - 0.0).abs() < f32::EPSILON,
        "confiance doit être 0.0 en mode reject"
    );
}

/// "admit-pending-review" (soft) : LLM timeout → final = PendingReview + fallback_applied.
#[tokio::test]
async fn fallback_admit_pending_review_soft() {
    let note = build_note_with_body("ambiguous");
    let curator = curator_with_fallback("admit-pending-review");
    let decision = curator.decide(&note, &CuratorContext::default()).await;

    assert_eq!(decision.final_status, NoteStatus::PendingReview);
    assert!(decision.fallback_applied);
    assert!(
        decision.reason.contains("admit pending review"),
        "reason doit mentionner 'admit pending review', obtenu : {}",
        decision.reason
    );
}
