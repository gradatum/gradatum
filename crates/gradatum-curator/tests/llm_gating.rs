//! Test : gating LLM — confiance faible → appel LLM → verdict LLM retourné.

mod common;
use common::build_note_with_body;

use async_trait::async_trait;
use gradatum_chat::{Chat, ChatBackend, ChatError, CuratorContext, CuratorVerdict, Heuristic};
use gradatum_core::config::CuratorConfig;
use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;
use gradatum_curator::Curator;
use std::sync::Arc;

/// Backend mock retournant un verdict fixe avec confiance 0.9.
struct MockLlm {
    verdict: CuratorVerdict,
}

#[async_trait]
impl Chat for MockLlm {
    async fn classify_curator(
        &self,
        _note: &Note,
        _ctx: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError> {
        Ok(self.verdict.clone())
    }

    fn backend_kind(&self) -> ChatBackend {
        ChatBackend::Http
    }
}

/// Body court → heuristique retourne 0.50 → LLM gating → MockLlm retourne `Live` à 0.9.
#[tokio::test]
async fn low_conf_calls_llm_and_uses_verdict() {
    // "short ambiguous" : < 50 chars → heuristic = 0.50, PendingReview
    let note = build_note_with_body("short ambiguous");
    let mock_llm = MockLlm {
        verdict: CuratorVerdict {
            proposed_status: NoteStatus::Live,
            confidence: 0.9,
            reason: "llm clear admit".into(),
            backend: ChatBackend::Http,
        },
    };
    let cfg = CuratorConfig {
        llm_review_enabled: Some(true),
        confidence_threshold: Some(0.7),
        ..Default::default()
    };
    let curator = Curator::new(Heuristic::new(), Some(Arc::new(mock_llm)), cfg);
    let decision = curator.decide(&note, &CuratorContext::default()).await;

    assert_eq!(
        decision.final_status,
        NoteStatus::Live,
        "LLM a tranché Live — le curator doit retourner Live"
    );
    assert_eq!(
        decision.backend_used,
        ChatBackend::Http,
        "backend_used doit refléter le backend LLM"
    );
    assert!(
        !decision.fallback_applied,
        "pas de fallback si LLM répond OK"
    );
    assert!(
        (decision.confidence - 0.9).abs() < f32::EPSILON,
        "confiance doit être celle du LLM"
    );
}
