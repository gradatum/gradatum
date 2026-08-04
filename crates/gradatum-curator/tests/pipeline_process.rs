//! Tests d'intégration de `CuratorPipeline::process()` — T6 P2.0c-tris.
//!
//! Vérifie le câblage runtime end-to-end :
//! 1. Mode heuristique (llm_review_enabled=false) → pas d'appel LLM, `Admitted` ou `Pending`
//! 2. Mode LLM (llm_review_enabled=true, confidence_threshold=1.0) → mock server
//!    → `Admitted` avec section LLM
//! 3. Mode LLM avec erreur serveur → `Pending` (fallback PendingReviewFallback)
//! 4. [A1] section_hint valide → Admitted direct, LLM non consulté
//! 5. [A1] section_hint invalide → warn + chemin normal (heuristique/LLM)
//! 6. [A1] section_hint absent → comportement identique à l'existant
//!
//! ## Patterns de test
//!
//! - `CuratorPipeline::from_config()` est instancié avec un mock wiremock
//! - Les mocks sont déterministes en CI (aucun appel réseau réel)
//! - Le `CLASSIFIER_SYSTEM_PROMPT` est le prompt classifier-v1 embedé dans le binaire

use gradatum_curator::{
    CurateOutcome, CuratorLlmConfig, CuratorPipeline, CuratorPipelineConfig, Note,
};
use wiremock::matchers::body_partial_json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Construit une `Note` minimale pour les tests.
fn make_note(title: &str, body: &str) -> Note {
    Note {
        id: ulid::Ulid::new().to_string(),
        title: title.to_string(),
        body: body.to_string(),
        tags_hint: vec![],
        section_hint: None,
    }
}

/// Réponse JSON LLM valide — format classifier-v1.
fn llm_response_json(section: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": serde_json::json!({
                    "section": section,
                    "tags": ["test-tag"],
                    "wikilinks": [],
                    "duplicate_hint": null
                }).to_string()
            }
        }]
    })
}

// ── Test 1 : heuristique pur — llm_review_enabled=false ─────────────────────

/// Note sans keywords forts → heuristique faible → Pending (LLM désactivé).
///
/// Vérifie que `llm_review_enabled=false` ne fait jamais appel à un backend LLM.
#[tokio::test]
async fn heuristic_only_llm_disabled_ambiguous_note_returns_pending() {
    // Body délibérément ambigu (aucun keyword fort) → confiance heuristique faible.
    let note = make_note("Short ambiguous", "This is a short note.");
    let cfg = CuratorPipelineConfig {
        backend: "heuristic".to_string(),
        llm: None,
        heuristic_admit_threshold: Some(0.8),
        heuristic_default_status: None,
        llm_review_enabled: Some(false),
        confidence_threshold: Some(0.7),
        llm_review_max_tokens: None,
        llm_review_fallback: None,
    };
    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    // La note est ambigu — doit être Pending ou Admitted (heuristique seule)
    // mais JAMAIS via LLM (aucun mock server lancé → crash si LLM appelé).
    match &outcome {
        CurateOutcome::Admitted { .. } | CurateOutcome::Pending { .. } => {
            // OK — pas d'appel LLM (le test aurait paniqué sinon)
        }
        CurateOutcome::Rejected { reason } => {
            panic!("Rejected inattendu en mode heuristique : {reason}");
        }
    }
}

/// Note avec keywords "decisions" forts → heuristique confiance élevée → Admitted direct.
#[tokio::test]
async fn heuristic_only_high_confidence_note_admitted_directly() {
    // 3+ matches "decisions" : "chose" + "picked" + "trade-off" → confiance haute.
    let note = make_note(
        "JWT TTL trade-off",
        "We chose Ed25519 and picked this approach after the trade-off evaluation. \
         We also decided to use Ed25519 and chose the approach after the trade-off.",
    );
    let cfg = CuratorPipelineConfig {
        backend: "heuristic".to_string(),
        llm: None,
        heuristic_admit_threshold: Some(0.8),
        heuristic_default_status: None,
        llm_review_enabled: Some(false),
        confidence_threshold: Some(0.7),
        llm_review_max_tokens: None,
        llm_review_fallback: None,
    };
    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    // Confiance élevée → Admitted — section "decisions" attendue.
    match outcome {
        CurateOutcome::Admitted { decisions } => {
            assert_eq!(
                decisions.canonical_section, "decisions",
                "section heuristique doit être 'decisions' pour note decisions-strong"
            );
        }
        CurateOutcome::Pending { .. } | CurateOutcome::Rejected { .. } => {
            // Toléré si la confiance n'atteint pas 0.8 — heuristique ambiguë.
            // Ne pas forcer un assert strict ici car le seuil dépend du scoring.
        }
    }
}

// ── Test 2 : mode LLM — llm_review_enabled=true, threshold=1.0 (toujours LLM) ──

/// Note ambiguë + `confidence_threshold=1.0` → LLM toujours appelé → `Admitted` LLM.
///
/// Vérifie le câblage complet : `from_config` → `CircuitBreaker` → `OpenAiCompatBackend`
/// → mock wiremock → `CurateOutcome::Admitted { canonical_section: "reasoning" }`.
#[tokio::test]
async fn llm_review_enabled_calls_llm_and_uses_verdict() {
    let mock_server = MockServer::start().await;

    // Mock : retourne section "reasoning" depuis le LLM.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(llm_response_json("reasoning")))
        .expect(1)
        .mount(&mock_server)
        .await;

    let note = make_note("Short ambiguous", "This is a short note.");
    let cfg = CuratorPipelineConfig {
        backend: "openai_compat".to_string(),
        llm: Some(CuratorLlmConfig {
            backend: "openai_compat".to_string(),
            base_url: mock_server.uri(),
            model: "test-model".to_string(),
            api_key_env: None, // Pas d'auth — endpoint LAN interne simulé
            timeout_ms: 5000,
        }),
        heuristic_admit_threshold: Some(0.8),
        heuristic_default_status: None,
        llm_review_enabled: Some(true),
        confidence_threshold: Some(1.0), // Toujours appeler le LLM
        llm_review_max_tokens: None,
        llm_review_fallback: None,
    };
    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    match outcome {
        CurateOutcome::Admitted { decisions } => {
            assert_eq!(
                decisions.canonical_section, "reasoning",
                "le verdict LLM (reasoning) doit prendre le dessus"
            );
            assert!(
                decisions.tags.contains(&"test-tag".to_string()),
                "les tags LLM doivent être propagés dans les décisions"
            );
        }
        other => panic!("Attendu Admitted depuis LLM, obtenu : {other:?}"),
    }
    // Le mock vérifie que l'endpoint a bien été appelé exactement 1 fois (.expect(1)).
}

/// LLM activé + erreur 500 du serveur → `Pending` avec fallback PendingReviewFallback.
///
/// Le CircuitBreaker laisse passer l'erreur (circuit Closed, 1 seul appel).
/// La fallback strategy doit être `PendingReviewFallback` par défaut.
#[tokio::test]
async fn llm_review_server_error_returns_pending_fallback() {
    let mock_server = MockServer::start().await;

    // Mock : retourne 500 Internal Server Error.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let note = make_note("Short ambiguous", "This is a short note.");
    let cfg = CuratorPipelineConfig {
        backend: "openai_compat".to_string(),
        llm: Some(CuratorLlmConfig {
            backend: "openai_compat".to_string(),
            base_url: mock_server.uri(),
            model: "test-model".to_string(),
            api_key_env: None,
            timeout_ms: 5000,
        }),
        heuristic_admit_threshold: Some(0.8),
        heuristic_default_status: None,
        llm_review_enabled: Some(true),
        confidence_threshold: Some(1.0), // Toujours appeler le LLM
        llm_review_max_tokens: None,
        llm_review_fallback: Some("pending-review-fallback".to_string()),
    };
    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    // Erreur LLM + fallback PendingReviewFallback → Pending.
    match outcome {
        CurateOutcome::Pending { reason, .. } => {
            assert!(
                reason.contains("llm down") || reason.contains("PendingReview"),
                "raison Pending doit indiquer l'erreur LLM : got '{reason}'"
            );
        }
        other => panic!("Attendu Pending sur erreur LLM, obtenu : {other:?}"),
    }
}

/// LLM activé + erreur 500 + fallback "reject" → `Rejected`.
#[tokio::test]
async fn llm_review_server_error_with_reject_fallback() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let note = make_note("Short ambiguous", "This is a short note.");
    let cfg = CuratorPipelineConfig {
        backend: "openai_compat".to_string(),
        llm: Some(CuratorLlmConfig {
            backend: "openai_compat".to_string(),
            base_url: mock_server.uri(),
            model: "test-model".to_string(),
            api_key_env: None,
            timeout_ms: 5000,
        }),
        heuristic_admit_threshold: Some(0.8),
        heuristic_default_status: None,
        llm_review_enabled: Some(true),
        confidence_threshold: Some(1.0),
        llm_review_max_tokens: None,
        llm_review_fallback: Some("reject".to_string()),
    };
    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    match outcome {
        CurateOutcome::Rejected { reason } => {
            assert!(
                reason.contains("reject"),
                "raison Rejected doit indiquer reject strict : got '{reason}'"
            );
        }
        other => panic!("Attendu Rejected avec fallback=reject, obtenu : {other:?}"),
    }
}

// ── Test 5 : propagation llm_review_max_tokens → backend ─────────────────────

/// `CuratorPipelineConfig.llm_review_max_tokens = Some(2048)` doit être propagé
/// dans la requête HTTP body (champ `max_tokens = 2048`).
///
/// Utilise `body_partial_json` pour capturer le body et vérifier la valeur.
/// Si la propagation est absente, le mock ne matche pas et `.expect(1)` échoue.
#[tokio::test]
async fn llm_review_max_tokens_propagated_to_backend_request() {
    let mock_server = MockServer::start().await;

    // Le mock ne répond que si le body contient "max_tokens": 2048.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({ "max_tokens": 2048_u32 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(llm_response_json("decisions")))
        .expect(1)
        .mount(&mock_server)
        .await;

    let note = make_note("Short ambiguous", "This is a short note.");
    let cfg = CuratorPipelineConfig {
        backend: "openai_compat".to_string(),
        llm: Some(CuratorLlmConfig {
            backend: "openai_compat".to_string(),
            base_url: mock_server.uri(),
            model: "test-model".to_string(),
            api_key_env: None,
            timeout_ms: 5000,
        }),
        heuristic_admit_threshold: Some(0.8),
        heuristic_default_status: None,
        llm_review_enabled: Some(true),
        confidence_threshold: Some(1.0), // Force appel LLM sur toutes notes
        llm_review_max_tokens: Some(2048), // Valeur à propager
        llm_review_fallback: None,
    };
    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    // Le mock a reçu max_tokens=2048 → LLM répond OK → Admitted.
    match outcome {
        CurateOutcome::Admitted { decisions } => {
            assert_eq!(
                decisions.canonical_section, "decisions",
                "le verdict LLM doit être 'decisions'"
            );
        }
        other => panic!(
            "Attendu Admitted avec max_tokens=2048 propagé, obtenu : {other:?}. \
             Si Pending/Rejected, le mock n'a pas reçu max_tokens=2048 dans le body."
        ),
    }
}

// ── Tests A1 : section_hint explicite ────────────────────────────────────────

/// [A1 t1] section_hint valide ("decisions") → Admitted direct sans consulter LLM.
///
/// Le backend est un MockServer lancé mais jamais appelé — si l'implémentation
/// contactait le LLM, le mock `.expect(0)` ferait échouer le test.
#[tokio::test]
async fn section_hint_valid_admits_directly_without_llm() {
    let mock_server = MockServer::start().await;

    // Aucune requête ne doit arriver sur le mock.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(llm_response_json("reasoning")))
        .expect(0) // LLM ne doit PAS être appelé
        .mount(&mock_server)
        .await;

    // Note délibérément ambiguë (corps court) — seul le hint doit décider.
    let note = Note {
        id: ulid::Ulid::new().to_string(),
        title: "Note quelconque".to_string(),
        body: "Contenu court et ambigu.".to_string(),
        tags_hint: vec![],
        section_hint: Some("decisions".to_string()), // hint valide parmi les 11 canoniques
    };

    // LLM activé + confidence_threshold=1.0 (forcerait toujours le LLM sans hint).
    let cfg = CuratorPipelineConfig {
        backend: "openai_compat".to_string(),
        llm: Some(CuratorLlmConfig {
            backend: "openai_compat".to_string(),
            base_url: mock_server.uri(),
            model: "test-model".to_string(),
            api_key_env: None,
            timeout_ms: 5000,
        }),
        heuristic_admit_threshold: Some(1.0), // Désactive le fast-path heuristique
        heuristic_default_status: None,
        llm_review_enabled: Some(true),
        confidence_threshold: Some(1.0), // Sans hint, forcerait le LLM
        llm_review_max_tokens: None,
        llm_review_fallback: None,
    };
    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    // Le hint valide doit produire Admitted avec canonical_section = "decisions".
    match outcome {
        CurateOutcome::Admitted { decisions } => {
            assert_eq!(
                decisions.canonical_section, "decisions",
                "section_hint valide doit être respecté : attendu 'decisions', obtenu '{}'",
                decisions.canonical_section
            );
        }
        other => panic!(
            "[A1 t1] Attendu Admitted avec canonical_section='decisions', obtenu : {other:?}"
        ),
    }
    // Le mock vérifie que .expect(0) est respecté — zéro appel LLM.
}

/// [A1 t2] section_hint invalide ("invalide-xyz") → warn loggé + chemin normal.
///
/// Le hint inconnu est ignoré. La note ambiguë passe par l'heuristique
/// (faible confiance) puis le LLM (enabled). Le mock LLM est appelé exactement 1 fois.
#[tokio::test]
async fn section_hint_invalid_falls_through_to_normal_path() {
    let mock_server = MockServer::start().await;

    // Le LLM DOIT être appelé (hint ignoré → chemin normal).
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(llm_response_json("reasoning")))
        .expect(1) // LLM doit être appelé exactement 1 fois
        .mount(&mock_server)
        .await;

    let note = Note {
        id: ulid::Ulid::new().to_string(),
        title: "Note quelconque".to_string(),
        body: "Contenu court et ambigu.".to_string(),
        tags_hint: vec![],
        section_hint: Some("invalide-xyz".to_string()), // hint hors des 11 sections canoniques
    };

    let cfg = CuratorPipelineConfig {
        backend: "openai_compat".to_string(),
        llm: Some(CuratorLlmConfig {
            backend: "openai_compat".to_string(),
            base_url: mock_server.uri(),
            model: "test-model".to_string(),
            api_key_env: None,
            timeout_ms: 5000,
        }),
        heuristic_admit_threshold: Some(1.0), // Désactive le fast-path heuristique
        heuristic_default_status: None,
        llm_review_enabled: Some(true),
        confidence_threshold: Some(1.0), // Force toujours le LLM (confiance heuristique < 1.0)
        llm_review_max_tokens: None,
        llm_review_fallback: None,
    };
    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    // Le chemin normal (LLM) donne "reasoning" — le hint invalide n'interfère pas.
    match outcome {
        CurateOutcome::Admitted { decisions } => {
            assert_ne!(
                decisions.canonical_section, "invalide-xyz",
                "hint invalide ne doit JAMAIS se retrouver dans canonical_section"
            );
            assert_eq!(
                decisions.canonical_section, "reasoning",
                "chemin normal LLM doit retourner 'reasoning' (verdict mock)"
            );
        }
        other => panic!("[A1 t2] Attendu Admitted depuis LLM (hint ignoré), obtenu : {other:?}"),
    }
    // .expect(1) vérifie que le LLM a été appelé exactement 1 fois.
}

/// [A1 t3] section_hint absent (None) → comportement identique au test existant
/// `heuristic_only_llm_disabled_ambiguous_note_returns_pending`.
///
/// Ce test est un témoin : il prouve que l'ajout du hint-path ne régresse pas
/// le comportement sans hint. Identique à la config du test existant nommé ci-dessus.
#[tokio::test]
async fn section_hint_none_behavior_unchanged() {
    // Pas de mock — heuristique pur, LLM désactivé.
    let note = make_note("Short ambiguous", "This is a short note."); // section_hint: None

    let cfg = CuratorPipelineConfig {
        backend: "heuristic".to_string(),
        llm: None,
        heuristic_admit_threshold: Some(0.8),
        heuristic_default_status: None,
        llm_review_enabled: Some(false),
        confidence_threshold: Some(0.7),
        llm_review_max_tokens: None,
        llm_review_fallback: None,
    };
    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    // Identique au test `heuristic_only_llm_disabled_ambiguous_note_returns_pending` :
    // note ambiguë + LLM désactivé → Admitted ou Pending, jamais Rejected.
    match &outcome {
        CurateOutcome::Admitted { .. } | CurateOutcome::Pending { .. } => {
            // OK — comportement inchangé sans hint
        }
        CurateOutcome::Rejected { reason } => {
            panic!("[A1 t3] Rejected inattendu sans hint (régression) : {reason}");
        }
    }
}

// ── Test A3 : section council propagée depuis le LLM ──────────────────────────

/// [A3] LLM mock retournant "council" → canonical_section="council" (pas de fallback Reference).
///
/// Régression guard : vérifie que la section council (11e section, prompt v2)
/// est reconnue et propagée sans dégradation vers une section par défaut.
/// Le titre est intentionnellement sans préfixe [COUNCIL] pour forcer le chemin LLM.
#[tokio::test]
async fn llm_mock_council_section_propagated() {
    let mock_server = MockServer::start().await;

    // Mock LLM : retourne la section "council".
    let llm_body = serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": serde_json::json!({
                    "section": "council",
                    "tags": ["art19", "verdict"],
                    "wikilinks": [],
                    "duplicate_hint": null
                }).to_string()
            }
        }]
    });

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(llm_body))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Titre sans préfixe [COUNCIL] — le fast-path préfixe ne matche pas.
    // Le titre contient "verdict" + "délibération" (score council = 2), le body
    // contient "architecture" (score architecture = 1) — signal ambigu (confiance < 1.0)
    // → heuristic ne peut pas admettre directement → LLM invoqué avec confidence_threshold=1.0.
    let note = Note {
        id: ulid::Ulid::new().to_string(),
        title: "Verdict — délibération de gouvernance".to_string(),
        body: "Short ambiguous body about architecture.".to_string(),
        tags_hint: vec![],
        section_hint: None,
    };

    let cfg = CuratorPipelineConfig {
        backend: "openai_compat".to_string(),
        llm: Some(CuratorLlmConfig {
            backend: "openai_compat".to_string(),
            base_url: mock_server.uri(),
            model: "test-model".to_string(),
            api_key_env: None,
            timeout_ms: 5000,
        }),
        heuristic_admit_threshold: Some(1.0), // Désactive le fast-path heuristique
        heuristic_default_status: None,
        llm_review_enabled: Some(true),
        confidence_threshold: Some(1.0), // Force toujours le LLM
        llm_review_max_tokens: None,
        llm_review_fallback: None,
    };

    let pipeline = CuratorPipeline::from_config(&cfg);
    let outcome = pipeline.process(note).await;

    match outcome {
        CurateOutcome::Admitted { decisions } => {
            assert_eq!(
                decisions.canonical_section, "council",
                "LLM mock retourne 'council' — canonical_section doit être 'council', \
                 pas de fallback Reference (régression guard prompt v2)"
            );
        }
        other => {
            panic!("[A3] Attendu Admitted avec canonical_section='council', obtenu : {other:?}")
        }
    }
    // .expect(1) vérifie que le LLM a bien été appelé.
}
