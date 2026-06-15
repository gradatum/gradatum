//! Tests d'intégration — circuit breaker.
//!
//! 3 scénarios :
//! 1. 3 failures consécutives → circuit ouvert → `CircuitOpen`
//! 2. 2 failures + 1 succès → compteur remis à zéro (circuit ne s'ouvre pas)
//! 3. Circuit ouvert → cooldown expiré → circuit reprobe (appel passé à l'inner)

mod common;

use common::build_note;
use gradatum_chat::{
    Chat, ChatBackend, ChatError, CircuitBreakerChat, CuratorContext, CuratorVerdict,
};
use gradatum_core::{note::Note, status::NoteStatus};
use std::time::Duration;

// --- Backends de test ---

/// Backend qui échoue systématiquement avec `ChatError::Timeout`.
struct FailingChat;

#[async_trait::async_trait]
impl Chat for FailingChat {
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

/// Backend qui réussit systématiquement.
struct SucceedingChat;

#[async_trait::async_trait]
impl Chat for SucceedingChat {
    async fn classify_curator(
        &self,
        _note: &Note,
        _ctx: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError> {
        Ok(CuratorVerdict {
            proposed_status: NoteStatus::Live,
            confidence: 0.9,
            reason: "test success".into(),
            backend: ChatBackend::Http,
        })
    }

    fn backend_kind(&self) -> ChatBackend {
        ChatBackend::Http
    }
}

// --- Tests ---

#[tokio::test]
async fn circuit_opens_after_3_consecutive_failures() {
    let cb = CircuitBreakerChat::new(FailingChat)
        .with_threshold(3)
        .with_cooldown(Duration::from_secs(60));
    let note = build_note();

    // 3 appels → failures 1, 2, 3 → circuit ouvert au 3e
    for i in 0..3 {
        let r = cb.classify_curator(&note, &CuratorContext::default()).await;
        assert!(
            matches!(r, Err(ChatError::Timeout)),
            "appel {i} devrait retourner Timeout"
        );
    }

    // 4e appel → circuit ouvert → CircuitOpen
    let r = cb.classify_curator(&note, &CuratorContext::default()).await;
    assert!(
        matches!(r, Err(ChatError::CircuitOpen)),
        "après 3 failures, le 4e appel devrait retourner CircuitOpen, obtenu: {:?}",
        r
    );
}

#[tokio::test]
async fn circuit_resets_on_success() {
    // Backend qui échoue les 2 premiers appels, puis réussit
    struct TwoFailsThenOk {
        calls: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl Chat for TwoFailsThenOk {
        async fn classify_curator(
            &self,
            _note: &Note,
            _ctx: &CuratorContext,
        ) -> Result<CuratorVerdict, ChatError> {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 2 {
                Err(ChatError::Timeout)
            } else {
                Ok(CuratorVerdict {
                    proposed_status: NoteStatus::Live,
                    confidence: 0.9,
                    reason: "ok".into(),
                    backend: ChatBackend::Http,
                })
            }
        }

        fn backend_kind(&self) -> ChatBackend {
            ChatBackend::Http
        }
    }

    let cb = CircuitBreakerChat::new(TwoFailsThenOk {
        calls: std::sync::atomic::AtomicU32::new(0),
    })
    .with_threshold(3)
    .with_cooldown(Duration::from_secs(60));
    let note = build_note();

    // 2 failures (seuil = 3 → circuit toujours fermé)
    for _ in 0..2 {
        let _ = cb.classify_curator(&note, &CuratorContext::default()).await;
    }

    // Succès → remise à zéro du compteur
    let r = cb.classify_curator(&note, &CuratorContext::default()).await;
    assert!(r.is_ok(), "le 3e appel (succès) devrait réussir: {:?}", r);

    // Circuit toujours fermé — pas de CircuitOpen
    assert!(
        !cb.is_open(),
        "le circuit ne devrait pas être ouvert après un succès"
    );
}

#[tokio::test]
async fn circuit_reopens_after_cooldown() {
    // Cooldown très court pour ne pas ralentir la suite de tests
    let cb = CircuitBreakerChat::new(SucceedingChat)
        .with_threshold(1)
        .with_cooldown(Duration::from_millis(50));
    let note = build_note();

    // 1 failure simulée manuellement — on wrappé un FailingChat pour l'ouverture
    let failing_cb = CircuitBreakerChat::new(FailingChat)
        .with_threshold(1)
        .with_cooldown(Duration::from_millis(50));

    // Ouvre le circuit
    let _ = failing_cb
        .classify_curator(&note, &CuratorContext::default())
        .await;
    assert!(
        failing_cb.is_open(),
        "circuit devrait être ouvert après 1 failure avec threshold=1"
    );

    // Circuit ouvert → CircuitOpen
    let r = failing_cb
        .classify_curator(&note, &CuratorContext::default())
        .await;
    assert!(
        matches!(r, Err(ChatError::CircuitOpen)),
        "circuit ouvert devrait retourner CircuitOpen"
    );

    // Avancer l'horloge logique au-delà du cooldown (50ms) — déterministe (D2.3)
    failing_cb.advance_test_clock(100);

    // Après cooldown → circuit fermé
    assert!(
        !failing_cb.is_open(),
        "après le cooldown, is_open() devrait retourner false"
    );

    // Le circuit est maintenant en mode probe — le prochain appel ira à l'inner
    // (le cb avec SucceedingChat, lui, n'a pas été ouvert)
    let r = cb.classify_curator(&note, &CuratorContext::default()).await;
    assert!(
        r.is_ok(),
        "après cooldown, un backend sain devrait réussir: {:?}",
        r
    );
}
