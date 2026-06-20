//! v1-parity : Curator workflow — 4 tests.
//!
//! Parité avec `legacy-vault-v1/tests/integration/test_gatekeeper.rs`.
//! Domaine : heuristic gating, LLM route low-conf, fallback, llm_disabled.

mod common;

use std::sync::Arc;

use gradatum_chat::{Chat, ChatBackend, CuratorContext, CuratorVerdict, Heuristic, Noop};
use gradatum_core::config::CuratorConfig;
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::status::NoteStatus;
use gradatum_curator::workflow::Curator;

use async_trait::async_trait;
use gradatum_chat::error::ChatError;

// --- Helpers ---

/// Construit une note avec un corpus "décision" long → confiance heuristique élevée.
fn note_high_confidence() -> Note {
    let fm = common::minimal_frontmatter("main");
    let body = "Cette note documente une décision architecturale importante concernant \
                l'usage de OpenDAL pour le storage layer. Après analyse des alternatives \
                (filesystem direct, S3, custom), OpenDAL offre le meilleur compromis \
                portabilité/performance. Decision retenue.";
    let hash = ContentHash::compute(&fm, body);
    Note {
        id: NoteId::new(),
        frontmatter: fm,
        body: NoteBody {
            markdown: body.into(),
        },
        version: NoteVersion::initial(),
        content_hash: hash,
        integrity_signature: None,
    }
}

/// Construit une note courte → confiance heuristique faible.
fn note_low_confidence() -> Note {
    let fm = common::minimal_frontmatter("main");
    // Corps exactement 60 chars — dépasse short_body_threshold=50 mais sans signal fort
    let body = "Note brève sans signal sémantique fort pour test low-conf.";
    let hash = ContentHash::compute(&fm, body);
    Note {
        id: NoteId::new(),
        frontmatter: fm,
        body: NoteBody {
            markdown: body.into(),
        },
        version: NoteVersion::initial(),
        content_hash: hash,
        integrity_signature: None,
    }
}

fn empty_context() -> CuratorContext {
    CuratorContext {
        similar_note_ids: vec![],
        vault_tags: vec![],
    }
}

// --- Mock Chat : retourne toujours Live ---

struct MockLiveLlm;

#[async_trait]
impl Chat for MockLiveLlm {
    async fn classify_curator(
        &self,
        _note: &Note,
        _ctx: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError> {
        Ok(CuratorVerdict {
            proposed_status: NoteStatus::Live,
            confidence: 0.95,
            reason: "mock LLM → Live".into(),
            backend: ChatBackend::Http,
        })
    }

    fn backend_kind(&self) -> ChatBackend {
        ChatBackend::Http
    }
}

// --- Mock Chat : retourne toujours une erreur ---

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

// --- 1. curator_admits_clear_decision ---

/// Note avec corps "décision" long → heuristique high-conf → fast path → Live.
/// Aucun appel LLM attendu.
#[tokio::test]
async fn curator_admits_clear_decision() {
    let curator: Curator<Noop> = Curator::new(
        Heuristic::new(),
        None,
        CuratorConfig {
            confidence_threshold: Some(0.7),
            llm_review_enabled: Some(false),
            ..Default::default()
        },
    );

    let note = note_high_confidence();
    let decision = curator.decide(&note, &empty_context()).await;

    assert_eq!(
        decision.final_status,
        NoteStatus::Live,
        "Note décision claire doit être admise Live par l'heuristique"
    );
    assert_eq!(decision.backend_used, ChatBackend::Heuristic);
    assert!(!decision.fallback_applied);
    assert!(
        decision.confidence >= 0.7,
        "Confiance doit être ≥ threshold : {}",
        decision.confidence
    );
}

// --- 2. curator_routes_low_conf_to_llm ---

/// Note courte → faible confiance heuristique → LLM activé → MockLiveLlm → Live.
#[tokio::test]
async fn curator_routes_low_conf_to_llm() {
    let curator = Curator::new(
        Heuristic::new(),
        Some(Arc::new(MockLiveLlm)),
        CuratorConfig {
            confidence_threshold: Some(0.7),
            llm_review_enabled: Some(true),
            ..Default::default()
        },
    );

    let note = note_low_confidence();
    let decision = curator.decide(&note, &empty_context()).await;

    assert_eq!(
        decision.final_status,
        NoteStatus::Live,
        "LLM mock retourne Live — décision finale doit être Live"
    );
    assert_eq!(
        decision.backend_used,
        ChatBackend::Http,
        "Le backend utilisé doit être Http (LLM appelé)"
    );
    assert!(!decision.fallback_applied);
}

// --- 3. curator_falls_back_pending_review ---

/// Note courte + LLM activé + LLM KO → FallbackStrategy::PendingReviewFallback.
#[tokio::test]
async fn curator_falls_back_pending_review() {
    let curator = Curator::new(
        Heuristic::new(),
        Some(Arc::new(FailingLlm)),
        CuratorConfig {
            confidence_threshold: Some(0.7),
            llm_review_enabled: Some(true),
            llm_review_fallback: Some("pending-review-fallback".into()),
            ..Default::default()
        },
    );

    let note = note_low_confidence();
    let decision = curator.decide(&note, &empty_context()).await;

    assert_eq!(
        decision.final_status,
        NoteStatus::PendingReview,
        "LLM down → fallback PendingReview"
    );
    assert!(
        decision.fallback_applied,
        "fallback_applied doit être true quand LLM est KO"
    );
}

// --- 4. curator_disabled_llm_keeps_heuristic_verdict ---

/// Note courte + llm_review_enabled=false → heuristique PendingReview (pas de LLM).
#[tokio::test]
async fn curator_disabled_llm_keeps_heuristic_verdict() {
    let curator = Curator::new(
        Heuristic::new(),
        Some(Arc::new(MockLiveLlm)), // présent mais ne doit PAS être appelé
        CuratorConfig {
            confidence_threshold: Some(0.7),
            llm_review_enabled: Some(false), // désactivé
            ..Default::default()
        },
    );

    let note = note_low_confidence();
    let decision = curator.decide(&note, &empty_context()).await;

    // LLM désactivé → PendingReview heuristique, pas de Http backend
    assert_eq!(
        decision.final_status,
        NoteStatus::PendingReview,
        "LLM désactivé → PendingReview"
    );
    assert_eq!(
        decision.backend_used,
        ChatBackend::Heuristic,
        "Backend doit être Heuristic quand LLM est désactivé"
    );
    assert!(
        !decision.fallback_applied,
        "fallback_applied doit être false (LLM désactivé, pas d'échec)"
    );
}
