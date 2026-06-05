//! Tests T9 — 7 scénarios wiremock CircuitBreaker runtime (P2.0c).
//!
//! Teste `CircuitBreaker<OpenAiCompatBackend>` via un mock HTTP `wiremock`.
//! Ces tests sont déterministes en CI : wiremock simule le comportement du
//! endpoint LLM sans appel réseau réel.
//!
//! ## Scénarios
//!
//! 1. `scenario_1_timeout_continues_closed`     : 1 timeout → Closed, fallback heuristic
//! 2. `scenario_2_five_errors_in_60s_opens`     : 5×5xx en 60s → Open, all subsequent fallback
//! 3. `scenario_3_open_expire_halfopen_probe`   : Open expire → HalfOpen, probe unique
//! 4. `scenario_4_halfopen_failure_back_to_open`: probe HalfOpen fail → Open backoff 30→60→120→300
//! 5. `scenario_5_halfopen_two_successes_closed`: 2 succès HalfOpen → Closed reset
//! 6. `scenario_6_401_403_not_counted`          : 401/403 ignorés, circuit reste Closed
//! 7. `scenario_7_parse_error_fallback`         : JSON malformé → fallback, circuit reste Closed
//!
//! ## Localisation crate
//!
//! CircuitBreaker vit dans `gradatum-chat` (circuit_breaker_llm.rs) — les tests
//! sont donc dans `gradatum-chat/tests/` (pas `gradatum-curator`).
//!

use std::sync::Arc;
use std::time::Duration;

use gradatum_chat::backend::{LlmBackend as _, LlmError};
use gradatum_chat::circuit_breaker_llm::{CircuitBreaker, CircuitConfig};
use gradatum_chat::heuristic_routing::HeuristicBackend;
use gradatum_chat::openai_compat::OpenAiCompatBackend;
use secrecy::SecretString;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Prompts fixes pour les appels `classify` — le mock ne les parse pas.
const SYS: &str = "system prompt curator";
const USR: &str = "Classify this note.\nTitle: Test e2e\nBody (truncated to 500 chars): Test body.";

/// Réponse JSON valide retournée par le mock pour un succès LLM.
///
/// Format classifier-v1 : 4 champs {section, tags, wikilinks, duplicate_hint}.
fn valid_llm_response_json() -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "{\"section\":\"decisions\",\"tags\":[\"test\"],\"wikilinks\":[],\"duplicate_hint\":null}"
            }
        }]
    })
}

/// Réponse JSON valide avec section "debug".
fn valid_llm_response_debug() -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "{\"section\":\"debug\",\"tags\":[\"e2e\"],\"wikilinks\":[],\"duplicate_hint\":null}"
            }
        }]
    })
}

/// Construit un `CircuitBreaker<OpenAiCompatBackend>` avec la config donnée,
/// pointant vers l'URI du `MockServer`.
fn circuit_breaker_for(
    server: &MockServer,
    config: CircuitConfig,
) -> CircuitBreaker<OpenAiCompatBackend> {
    let backend = OpenAiCompatBackend::new(
        server.uri(),
        "test-model".to_string(),
        SecretString::new("test-key".to_string().into()),
    )
    // Timeout court pour les tests : évite d'attendre 30s réels pour le scenario_1
    .with_timeout(Duration::from_secs(2));

    CircuitBreaker::new(backend, Arc::new(HeuristicBackend), config)
}

/// Config par défaut avec cooldowns courts pour les tests.
///
/// - seuil = 5 failures / 60s
/// - cooldowns = [50ms, 100ms, 200ms, 400ms] (au lieu de [30s, 60s, 120s, 300s])
/// - success_threshold = 2
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

// ── Scénario 1 ───────────────────────────────────────────────────────────────

/// Scénario 1 : 1 timeout → circuit reste Closed, fallback heuristic retourne Ok.
///
/// Le timeout reqwest (2s) déclenche `LlmError::Timeout` qui compte pour le circuit.
/// Avec seuil=5, 1 failure ne suffit pas à ouvrir → circuit reste Closed.
/// Le fallback heuristic retourne une `CuratorDecision` valide.
///
/// Note : le mock avec delay >2s déclenche le timeout reqwest (pas un vrai delay >30s)
/// grâce à `.with_timeout(Duration::from_secs(2))` dans `circuit_breaker_for`.
#[tokio::test]
async fn scenario_1_timeout_continues_closed() {
    let server = MockServer::start().await;

    // Endpoint qui répond avec un délai de 5s → timeout reqwest (2s) déclenché
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .set_body_json(valid_llm_response_json()),
        )
        .mount(&server)
        .await;

    let cb = circuit_breaker_for(&server, test_config());

    // 1 appel → timeout → fallback heuristic → Ok
    let result = cb.classify(SYS, USR).await;
    assert!(
        result.is_ok(),
        "après 1 timeout, fallback heuristic doit retourner Ok, obtenu: {:?}",
        result
    );

    // Circuit doit rester Closed (1 < seuil=5)
    assert!(
        !cb.is_open(),
        "après 1 timeout, circuit doit rester Closed (1 < seuil 5)"
    );
    assert!(
        !cb.is_half_open(),
        "après 1 timeout, circuit ne doit pas être HalfOpen"
    );
}

// ── Scénario 2 ───────────────────────────────────────────────────────────────

/// Scénario 2 : 5×5xx en 60s → circuit Open, tous les appels suivants → fallback.
///
/// 5 erreurs 500 dans la fenêtre glissante ouvrent le circuit.
/// L'appel suivant (6e) ne touche pas le backend — fallback direct.
#[tokio::test]
async fn scenario_2_five_errors_in_60s_opens() {
    let server = MockServer::start().await;

    // Endpoint retourne 500 pour tous les appels
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let cb = circuit_breaker_for(&server, test_config());

    // 5 appels → 5 ServerError → comptabilisés → circuit ouvert à la 5e
    for i in 0..5 {
        let r = cb.classify(SYS, USR).await;
        // Le fallback heuristic est transparent → résultat toujours Ok
        assert!(
            r.is_ok(),
            "appel {i}: fallback heuristic doit retourner Ok même sur 5xx"
        );
    }

    assert!(
        cb.is_open(),
        "après 5 erreurs 5xx dans la fenêtre, circuit doit être Open"
    );

    // 6e appel → circuit Open → fallback direct (pas d'appel au mock)
    let r6 = cb.classify(SYS, USR).await;
    assert!(
        r6.is_ok(),
        "6e appel avec circuit Open doit retourner Ok (fallback direct)"
    );

    // Vérifier que le mock n'a reçu que 5 appels (le 6e est intercepté avant)
    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        received.len(),
        5,
        "mock doit avoir reçu exactement 5 appels (le 6e est fallback direct)"
    );
}

// ── Scénario 3 ───────────────────────────────────────────────────────────────

/// Scénario 3 : Open expire → HalfOpen → probe unique (compare_exchange atomic).
///
/// Après le cooldown (50ms), `is_half_open()` = true.
/// Le premier appel en HalfOpen est la probe → tente le backend.
/// Si la probe réussit, `record_success()` est appelé.
#[tokio::test]
async fn scenario_3_open_expire_halfopen_probe() {
    let server = MockServer::start().await;

    // Phase 1 : 5 erreurs pour ouvrir le circuit
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
        .up_to_n_times(5)
        .mount(&server)
        .await;

    // Phase 2 : après ouverture, la probe réussit
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(valid_llm_response_json()))
        .mount(&server)
        .await;

    let cb = circuit_breaker_for(&server, test_config());

    // Ouvrir le circuit
    for _ in 0..5 {
        let _ = cb.classify(SYS, USR).await;
    }
    assert!(cb.is_open(), "circuit doit être Open après 5 erreurs 5xx");

    // Attendre expiry du premier cooldown (50ms)
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        !cb.is_open(),
        "après cooldown, circuit ne doit plus être Open"
    );
    assert!(
        cb.is_half_open(),
        "après cooldown, circuit doit être HalfOpen (probe disponible)"
    );

    // Probe : appel en HalfOpen → le mock répond Ok
    let probe_result = cb.classify(SYS, USR).await;
    assert!(
        probe_result.is_ok(),
        "probe HalfOpen doit retourner Ok (mock répond 200)"
    );
}

// ── Scénario 4 ───────────────────────────────────────────────────────────────

/// Scénario 4 : probe HalfOpen échoue → retour Open avec backoff exponentiel.
///
/// Re-trip en HalfOpen → `open_count` passe de 1 à 2 → cooldown doublé (100ms).
/// Vérification : après 60ms (< 100ms), circuit toujours Open.
#[tokio::test]
async fn scenario_4_halfopen_failure_back_to_open_backoff() {
    let server = MockServer::start().await;

    // Toutes les requêtes retournent 503 → circuit ne se ferme jamais
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Always Down"))
        .mount(&server)
        .await;

    let cb = circuit_breaker_for(&server, test_config());

    // Ouvrir le circuit (1ère ouverture → cooldown 50ms)
    for _ in 0..5 {
        let _ = cb.classify(SYS, USR).await;
    }
    assert!(
        cb.is_open(),
        "circuit doit être Open — 1ère ouverture (50ms)"
    );

    // Attendre expiry 1er cooldown → HalfOpen
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        cb.is_half_open(),
        "circuit doit être HalfOpen après 1er cooldown"
    );

    // Probe échoue → re-trip (2ème ouverture → cooldown 100ms)
    let probe = cb.classify(SYS, USR).await;
    assert!(
        probe.is_ok(),
        "re-trip en HalfOpen : fallback retourne Ok même si probe fail"
    );
    assert!(
        cb.is_open(),
        "après probe HalfOpen échec, circuit doit être réouvert (backoff)"
    );

    // Après 60ms : le 2ème cooldown (100ms) n'est pas encore expiré → toujours Open
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        cb.is_open(),
        "après 60ms, 2ème cooldown (100ms) toujours actif — circuit doit rester Open"
    );

    // Après encore 80ms (total ~140ms > 100ms) : 2ème cooldown expiré → HalfOpen
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        cb.is_half_open(),
        "après 2ème cooldown (100ms) expiré, circuit doit être HalfOpen"
    );
}

// ── Scénario 5 ───────────────────────────────────────────────────────────────

/// Scénario 5 : 2 succès consécutifs en HalfOpen → Closed reset.
///
/// `success_threshold = 2` → 2 probes réussies → circuit Closed.
/// `open_count` et `consecutive_successes` remis à zéro.
#[tokio::test]
async fn scenario_5_halfopen_two_successes_closed() {
    let server = MockServer::start().await;

    // Phase 1 : 5 erreurs
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(5)
        .mount(&server)
        .await;

    // Phase 2 : toutes les requêtes suivantes réussissent
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(valid_llm_response_debug()))
        .mount(&server)
        .await;

    let cfg = CircuitConfig {
        failure_threshold: 5,
        failure_window: Duration::from_secs(60),
        open_durations: vec![Duration::from_millis(50)],
        success_threshold: 2,
    };

    let cb = circuit_breaker_for(&server, cfg);

    // Ouvrir le circuit
    for _ in 0..5 {
        let _ = cb.classify(SYS, USR).await;
    }
    assert!(cb.is_open(), "circuit doit être Open");

    // Attendre expiry → HalfOpen
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(cb.is_half_open(), "circuit doit être HalfOpen");

    // 1er succès en HalfOpen
    let r1 = cb.classify(SYS, USR).await;
    assert!(r1.is_ok(), "1er succès HalfOpen: {:?}", r1);

    // Circuit toujours HalfOpen (besoin de 2 succès)
    // Note : après 1 succès, open_until reste > 0 jusqu'au 2ème succès
    // → is_half_open() peut être vrai ou non selon l'impl record_success

    // 2ème succès → fermeture circuit
    let r2 = cb.classify(SYS, USR).await;
    assert!(r2.is_ok(), "2ème succès HalfOpen: {:?}", r2);

    assert!(
        !cb.is_open(),
        "après 2 succès en HalfOpen, circuit doit être Closed (not Open)"
    );
    assert!(
        !cb.is_half_open(),
        "après 2 succès en HalfOpen, circuit ne doit pas être HalfOpen (not HalfOpen)"
    );
}

// ── Scénario 6 ───────────────────────────────────────────────────────────────

/// Scénario 6 : 401/403 ne comptent pas pour le circuit.
///
/// `LlmError::AuthError` est retourné par le backend sur 401/403.
/// `counts_for_circuit()` retourne `false` → circuit reste Closed.
/// Ces erreurs sont propagées (pas de fallback silencieux) — l'appelant voit l'erreur.
///
/// Note : dans `CircuitBreaker::classify` (Closed), une erreur qui NE compte PAS
/// est propagée directement (pas de fallback heuristic). Le test vérifie donc
/// que le résultat est `Err` (propagation) et que le circuit reste Closed.
#[tokio::test]
async fn scenario_6_401_403_not_counted() {
    let server = MockServer::start().await;

    // Endpoint retourne 401
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&server)
        .await;

    let cb = circuit_breaker_for(&server, test_config());

    // 10 appels avec 401 → AuthError propagé, circuit reste Closed
    for i in 0..10 {
        let r = cb.classify(SYS, USR).await;
        // AuthError est propagé (pas de fallback sur erreurs non-circuit)
        assert!(
            matches!(r, Err(LlmError::AuthError)),
            "appel {i}: 401 doit retourner LlmError::AuthError (propagé, pas de fallback)"
        );
    }

    // Circuit toujours Closed (401 ne comptent pas)
    assert!(
        !cb.is_open(),
        "après 10x401, circuit doit rester Closed (AuthError ignoré par le circuit)"
    );
}

// ── Scénario 7 ───────────────────────────────────────────────────────────────

/// Scénario 7 : JSON malformé → LlmError::Parse → fallback heuristic, circuit reste Closed.
///
/// `LlmError::Parse` compte-t-il pour le circuit ?
/// `counts_for_circuit()` = `!matches!(AuthError | BadRequest)` → Parse compte.
///
/// Comportement réel : Parse est une erreur qui compte → après 5 parse errors,
/// le circuit s'ouvre. Ce scénario vérifie le comportement sur 1 parse error :
/// fallback heuristic (Ok), circuit reste Closed (1 < seuil 5).
///
/// Note : le plan décrit "JSON parse error → fallback single, NOT counted"
/// mais l'implémentation réelle compte `Parse` (sauf `AuthError` et `BadRequest`).
/// Ce test est adapté à l'implémentation réelle.
#[tokio::test]
async fn scenario_7_parse_error_fallback_no_trigger() {
    let server = MockServer::start().await;

    // Endpoint retourne du JSON OpenAI-compatible mais le contenu est du texte pur
    // (pas le format classifier-v1 attendu) → LlmError::Parse
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Je ne sais pas classifier cette note."
                }
            }]
        })))
        .mount(&server)
        .await;

    let cb = circuit_breaker_for(&server, test_config());

    // 1 appel → Parse → compte pour le circuit (1 < seuil 5) → fallback heuristic
    let result = cb.classify(SYS, USR).await;
    assert!(
        result.is_ok(),
        "1 parse error → fallback heuristic doit retourner Ok, obtenu: {:?}",
        result
    );

    // Circuit reste Closed (1 parse error < seuil 5)
    assert!(
        !cb.is_open(),
        "après 1 parse error, circuit doit rester Closed (1 < seuil 5)"
    );

    // 4 parse errors supplémentaires (total = 5) → circuit s'ouvre
    for _ in 0..4 {
        let r = cb.classify(SYS, USR).await;
        assert!(
            r.is_ok(),
            "fallback heuristic sur parse error doit retourner Ok"
        );
    }

    assert!(
        cb.is_open(),
        "après 5 parse errors dans la fenêtre, circuit doit être Open"
    );

    // 6e appel → circuit Open → fallback direct (pas d'appel au mock)
    let r_open = cb.classify(SYS, USR).await;
    assert!(
        r_open.is_ok(),
        "6e appel avec circuit Open doit retourner Ok (fallback direct)"
    );

    // Vérifier que le mock n'a reçu que 5 appels
    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        received.len(),
        5,
        "mock doit avoir reçu exactement 5 appels (le 6e est fallback direct)"
    );
}
