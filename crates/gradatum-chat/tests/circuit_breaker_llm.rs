//! Tests d'intégration — `CircuitBreaker<B: LlmBackend>` Phase 2.
//!
//! 7 scénarios :
//! 1. `closed_stays_closed` : 5 succès → circuit reste fermé
//! 2. `closed_to_open_after_5_failures` : 5 failures → circuit ouvert, fallback retourne Ok
//! 3. `window_expiry_resets_failure_count` : failures expirées ne comptent plus
//! 4. `open_to_halfopen_after_timeout` : after cooldown, `is_half_open()` = true
//! 5. `halfopen_to_closed_with_2_successes` : 2 succès consécutifs en HalfOpen → Closed
//! 6. `halfopen_to_open_retrp_with_backoff` : re-trip en HalfOpen → backoff exponentiel
//! 7. `fallback_returns_valid_curator_decision` : Open → fallback shape identique au backend

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gradatum_chat::backend::{CuratorDecision, LlmBackend, LlmError};
use gradatum_chat::circuit_breaker_llm::{CircuitBreaker, CircuitConfig};
use gradatum_chat::heuristic_routing::HeuristicBackend;

// --- Helpers ---

fn ok_decision() -> CuratorDecision {
    CuratorDecision {
        section: "decisions".into(),
        tags: vec!["test".into()],
        wikilinks: vec![],
        duplicate_hint: None,
    }
}

const SYS: &str = "system prompt";
const USR: &str = "Classify this note.\nTitle: Decision JWT TTL\nBody (truncated to 500 chars): We decided to use Ed25519.";

// --- Backends de test ---

/// Backend qui réussit toujours.
struct AlwaysOk;

#[async_trait]
impl LlmBackend for AlwaysOk {
    fn name(&self) -> &'static str {
        "always_ok"
    }
    fn is_local(&self) -> bool {
        true
    }
    async fn classify(&self, _s: &str, _u: &str) -> Result<CuratorDecision, LlmError> {
        Ok(ok_decision())
    }
}

/// Backend qui échoue toujours avec Timeout.
struct AlwaysFail;

#[async_trait]
impl LlmBackend for AlwaysFail {
    fn name(&self) -> &'static str {
        "always_fail"
    }
    fn is_local(&self) -> bool {
        true
    }
    async fn classify(&self, _s: &str, _u: &str) -> Result<CuratorDecision, LlmError> {
        Err(LlmError::Timeout)
    }
}

/// Backend qui échoue les N premiers appels, puis réussit.
struct FailThenOk {
    count: tokio::sync::Mutex<u32>,
    fail_n: u32,
}

#[async_trait]
impl LlmBackend for FailThenOk {
    fn name(&self) -> &'static str {
        "fail_then_ok"
    }
    fn is_local(&self) -> bool {
        true
    }
    async fn classify(&self, _s: &str, _u: &str) -> Result<CuratorDecision, LlmError> {
        let mut c = self.count.lock().await;
        let n = *c;
        *c += 1;
        if n < self.fail_n {
            Err(LlmError::Timeout)
        } else {
            Ok(ok_decision())
        }
    }
}

/// Config avec cooldowns courts pour les tests.
fn test_config() -> CircuitConfig {
    CircuitConfig {
        failure_threshold: 5,
        failure_window: Duration::from_secs(60),
        open_durations: vec![
            Duration::from_millis(50),
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(400),
        ],
        success_threshold: 2,
    }
}

// --- Tests ---

/// Test 1 : 5 succès → circuit reste fermé
#[tokio::test]
async fn closed_stays_closed_under_5_successes() {
    let cb = CircuitBreaker::new(
        AlwaysOk,
        Arc::new(HeuristicBackend),
        CircuitConfig::default(),
    );
    for _ in 0..5 {
        let r = cb.classify(SYS, USR).await;
        assert!(r.is_ok(), "succès attendu: {:?}", r);
    }
    assert!(!cb.is_open(), "circuit ne doit pas s'ouvrir sur 5 succès");
}

/// Test 2 : 5 failures → circuit ouvert, fallback retourne Ok (not Err)
#[tokio::test]
async fn closed_to_open_after_5_failures() {
    let cb = CircuitBreaker::new(
        AlwaysFail,
        Arc::new(HeuristicBackend),
        CircuitConfig::default(),
    );
    for i in 0..5 {
        // Le fallback heuristic retourne Ok même si le backend interne échoue
        let r = cb.classify(SYS, USR).await;
        assert!(r.is_ok(), "appel {i}: fallback heuristic doit retourner Ok");
    }
    assert!(cb.is_open(), "circuit doit être ouvert après 5 failures");
}

/// Test 3 : failures expirées ne comptent plus dans la fenêtre
#[tokio::test]
async fn window_expiry_resets_failure_count() {
    let cfg = CircuitConfig {
        failure_threshold: 5,
        failure_window: Duration::from_millis(50),
        open_durations: vec![Duration::from_secs(60)],
        success_threshold: 2,
    };
    let cb = CircuitBreaker::new(AlwaysFail, Arc::new(HeuristicBackend), cfg);

    // 4 failures (seuil = 5)
    for _ in 0..4 {
        let _ = cb.classify(SYS, USR).await;
    }
    // Attendre expiry de la fenêtre (50ms)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4 nouvelles failures — les précédentes ont expiré
    for _ in 0..4 {
        let _ = cb.classify(SYS, USR).await;
    }
    assert!(
        !cb.is_open(),
        "après expiry fenêtre, 4 nouvelles failures ne doivent pas ouvrir (seuil=5)"
    );
}

/// Test 4 : après cooldown, circuit passe en HalfOpen
#[tokio::test]
async fn open_to_halfopen_after_timeout() {
    let cfg = test_config();
    let cb = CircuitBreaker::new(AlwaysFail, Arc::new(HeuristicBackend), cfg);

    // Ouvre le circuit
    for _ in 0..5 {
        let _ = cb.classify(SYS, USR).await;
    }
    assert!(cb.is_open(), "circuit doit être ouvert");

    // Attendre expiry du premier cooldown (50ms)
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        !cb.is_open(),
        "après cooldown, circuit ne doit plus être open"
    );
    assert!(
        cb.is_half_open(),
        "circuit doit être half-open après expiry du cooldown"
    );
}

/// Test 5 : 2 succès consécutifs en HalfOpen → Closed
#[tokio::test]
async fn halfopen_to_closed_with_2_successes() {
    let cfg = CircuitConfig {
        failure_threshold: 5,
        failure_window: Duration::from_secs(60),
        open_durations: vec![Duration::from_millis(50)],
        success_threshold: 2,
    };
    let cb = CircuitBreaker::new(
        FailThenOk {
            count: tokio::sync::Mutex::new(0),
            fail_n: 5,
        },
        Arc::new(HeuristicBackend),
        cfg,
    );

    // Ouvre le circuit
    for _ in 0..5 {
        let _ = cb.classify(SYS, USR).await;
    }
    assert!(cb.is_open());

    // Attendre expiry → HalfOpen
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(cb.is_half_open());

    // 1er succès en HalfOpen
    let r1 = cb.classify(SYS, USR).await;
    assert!(r1.is_ok(), "1er succès HalfOpen: {:?}", r1);

    // 2ème succès → Closed
    let r2 = cb.classify(SYS, USR).await;
    assert!(r2.is_ok(), "2ème succès HalfOpen: {:?}", r2);

    assert!(!cb.is_open(), "après 2 succès, circuit doit être fermé");
    assert!(
        !cb.is_half_open(),
        "après 2 succès, circuit ne doit plus être half-open"
    );
}

/// Test 6 : re-trip en HalfOpen → backoff exponentiel (open_count augmente)
#[tokio::test]
async fn halfopen_to_open_retrp_with_backoff() {
    let cfg = test_config();
    let cb = CircuitBreaker::new(AlwaysFail, Arc::new(HeuristicBackend), cfg);

    // 1ère ouverture
    for _ in 0..5 {
        let _ = cb.classify(SYS, USR).await;
    }
    assert!(cb.is_open(), "circuit doit être ouvert");

    // Attendre expiry → HalfOpen (50ms)
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(cb.is_half_open());

    // Re-trip : une failure en HalfOpen → réouvre avec backoff
    let _ = cb.classify(SYS, USR).await;

    assert!(
        cb.is_open(),
        "circuit doit être réouvert après re-trip en HalfOpen"
    );

    // open_count doit être > 1 (backoff exponentiel actif)
    // Note: open_count est privé, on vérifie indirectement via is_open()
    // après expiry du premier cooldown (50ms) le circuit sera encore ouvert
    // car le 2ème cooldown est 100ms
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        cb.is_open(),
        "avec backoff exponentiel, le 2ème cooldown (100ms) doit toujours être actif après 60ms"
    );
}

/// Test 7 : fallback retourne une CuratorDecision valide (même shape que le backend)
#[tokio::test]
async fn fallback_returns_valid_curator_decision() {
    let cfg = CircuitConfig {
        failure_threshold: 1,
        failure_window: Duration::from_secs(60),
        open_durations: vec![Duration::from_secs(60)],
        success_threshold: 2,
    };
    let cb = CircuitBreaker::new(AlwaysFail, Arc::new(HeuristicBackend), cfg);

    // Ouvre le circuit avec 1 failure
    let r1 = cb.classify(SYS, USR).await;
    assert!(r1.is_ok(), "fallback sur 1ère failure: {:?}", r1);
    assert!(cb.is_open(), "circuit doit être ouvert");

    // Fallback retourne une décision valide
    let decision = cb.classify(SYS, USR).await.unwrap();
    assert!(
        !decision.section.is_empty(),
        "fallback heuristic doit retourner une section non vide, obtenu: {:?}",
        decision
    );

    // La décision a la même structure qu'une décision normale
    // (tags peut être vide pour l'heuristique, c'est normal)
    assert!(
        matches!(
            decision.section.as_str(),
            "decisions"
                | "architecture"
                | "debug"
                | "reasoning"
                | "feedback"
                | "lessons-learned"
                | "retrospectives"
                | "experiments"
                | "agent-issues"
                | "reference"
        ),
        "section doit être une section canonique gradatum, obtenu: {:?}",
        decision.section
    );
}
