//! Tests d'intégration du routeur de raisonnement (`[router].enabled = true`).
//!
//! Exerce le chemin COMPLET de `POST /v1/chat/completions` avec le routeur activé,
//! le curateur simulé par `wiremock` (aucun engine LIVE). Vérifie, en bout-de-chaîne,
//! le corps effectivement forwardé au provider :
//! - le flag `chat_template_kwargs.enable_thinking` (axe raisonnement) ;
//! - le preset de sampling per-mode (temp / top_p / top_k / presence_penalty).
//!
//! Cas couverts :
//! 1. frontière → curateur `THINK`  → enable_thinking = true + preset Think ;
//! 2. pré-classifié (greeting)       → AUCUN appel curateur + preset NoThink ;
//! 3. curateur en timeout            → fallback no-think (permit relâché — R1) ;
//! 4. override `X-Reasoning-Mode`     → court-circuite le routeur (0 appel curateur).
//!
//! Harness : `tower::ServiceExt::oneshot` (pas de serveur HTTP réel) ;
//! `AppState::for_test` construit le `RouterClient` depuis `config.router`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gradatum_gateway::config::{
    AliasTarget, Config, LoggingConfig, ProviderConfig, RouterConfig, ServerConfig,
    VaultAwareConfig,
};
use gradatum_gateway::{AppState, build_router};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Réponse chat.completion minimale valide (le handler la forward telle quelle → 200).
fn chat_completion_ok() -> Value {
    json!({
        "id": "chatcmpl-router-int",
        "object": "chat.completion",
        "created": 1234567890u64,
        "model": "agent-main-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

/// Config gateway : 1 provider (l'engine mocké) + 1 alias + routeur activé pointant
/// sur le curateur mocké.
fn config_router_on(provider_endpoint: &str, curator_endpoint: &str, timeout_ms: u64) -> Config {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "engine".to_string(),
        ProviderConfig {
            endpoint: provider_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );

    let mut aliases = std::collections::HashMap::new();
    aliases.insert(
        "agent-main".to_string(),
        AliasTarget::simple("engine", "agent-main-model"),
    );

    Config {
        server: ServerConfig {
            listen: "127.0.0.1:18436".to_string(),
            registry_db: None,
            bearer_token_env: None,
            rate_limit_per_minute: 1000,
            circuit_threshold: 5,
            circuit_window_secs: 60,
            circuit_cooldown_secs: 30,
            max_total_tokens: 0, // cap désactivé → pas de 413 parasite
            trust_localhost: true,
            enable_slot_passthrough: false,
            allowed_origins: vec![],
            max_tools_per_request: 64,
        },
        providers,
        aliases,
        gateway: std::collections::HashMap::new(),
        logging: LoggingConfig::default(),
        vault_aware: VaultAwareConfig::default(),
        messages: Default::default(),
        router: RouterConfig {
            enabled: true,
            endpoint: curator_endpoint.to_string(),
            model: "curator".to_string(),
            timeout_ms,
            max_concurrent: 1,
            query_head_chars: 384,
        },
    }
}

/// Monte un curateur wiremock renvoyant le label donné (`THINK` / `NO_THINK`).
async fn mount_curator(server: &MockServer, label: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "content": label } }]
        })))
        .mount(server)
        .await;
}

/// Monte le provider (engine) wiremock renvoyant une complétion valide.
async fn mount_provider(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_completion_ok()))
        .mount(server)
        .await;
}

/// Envoie une requête chat au gateway, avec un header `X-Reasoning-Mode` optionnel.
async fn post_chat(config: Config, user_query: &str, reasoning_header: Option<&str>) -> StatusCode {
    let state = AppState::for_test(config);
    let app = build_router(state);

    let body = json!({
        "model": "agent-main",
        "messages": [{"role": "user", "content": user_query}]
    });

    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(h) = reasoning_header {
        builder = builder.header("x-reasoning-mode", h);
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();

    app.oneshot(req).await.unwrap().status()
}

/// Extrait le corps JSON de l'unique requête reçue par un mock (ou panique).
async fn single_forwarded_body(server: &MockServer) -> Value {
    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        received.len(),
        1,
        "exactement 1 requête forwardée attendue, reçu {}",
        received.len()
    );
    serde_json::from_slice(&received[0].body).expect("corps forwardé JSON valide")
}

// ── Cas 1 : frontière → curateur THINK → enable_thinking + preset Think ─────────

#[tokio::test]
async fn router_on_frontiere_curator_think_forward_enable_thinking_et_preset() {
    let provider = MockServer::start().await;
    let curator = MockServer::start().await;
    mount_provider(&provider).await;
    mount_curator(&curator, "THINK").await;

    // "what port does example-dns use" = frontière (pas de trigger, pas de greeting/commande).
    let status = post_chat(
        config_router_on(&provider.uri(), &curator.uri(), 500),
        "what port does example-dns use",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "handler doit forwarder → 200");

    // Le curateur a bien été interrogé (chemin routeur emprunté).
    let curator_hits = curator.received_requests().await.unwrap_or_default();
    assert_eq!(
        curator_hits.len(),
        1,
        "curateur interrogé 1 fois (frontière)"
    );

    // Corps forwardé au provider : enable_thinking = true + preset Think.
    let fwd = single_forwarded_body(&provider).await;
    assert_eq!(
        fwd.pointer("/chat_template_kwargs/enable_thinking")
            .and_then(Value::as_bool),
        Some(true),
        "enable_thinking = true (curateur THINK)"
    );
    assert_eq!(
        fwd.get("temperature").and_then(Value::as_f64),
        Some(0.6),
        "preset Think : temperature 0.6"
    );
    assert_eq!(
        fwd.get("top_p").and_then(Value::as_f64),
        Some(0.95),
        "preset Think : top_p 0.95"
    );
    assert_eq!(
        fwd.get("top_k").and_then(Value::as_u64),
        Some(20),
        "preset Think : top_k 20"
    );
    assert_eq!(
        fwd.get("presence_penalty").and_then(Value::as_f64),
        Some(1.2),
        "preset Think : presence_penalty 1.2"
    );
}

// ── Cas 2 : pré-classifié (greeting) → AUCUN appel curateur + preset NoThink ─────

#[tokio::test]
async fn router_on_preclassifie_ne_touche_pas_le_curator_preset_nothink() {
    let provider = MockServer::start().await;
    let curator = MockServer::start().await;
    mount_provider(&provider).await;
    // Le curateur répondrait THINK — mais il ne DOIT PAS être appelé.
    mount_curator(&curator, "THINK").await;

    let status = post_chat(
        config_router_on(&provider.uri(), &curator.uri(), 500),
        "hello there",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Pré-classifieur : greeting → NO_THINK sans appel réseau.
    let curator_hits = curator.received_requests().await.unwrap_or_default();
    assert_eq!(
        curator_hits.len(),
        0,
        "greeting court-circuite le curateur (0 appel)"
    );

    let fwd = single_forwarded_body(&provider).await;
    assert_eq!(
        fwd.pointer("/chat_template_kwargs/enable_thinking")
            .and_then(Value::as_bool),
        Some(false),
        "enable_thinking = false (no-think)"
    );
    assert_eq!(
        fwd.get("temperature").and_then(Value::as_f64),
        Some(0.4),
        "preset NoThink : temperature 0.4"
    );
    assert_eq!(
        fwd.get("presence_penalty").and_then(Value::as_f64),
        Some(1.0),
        "preset NoThink : presence_penalty 1.0"
    );
}

// ── Cas 3 : curateur en timeout → fallback no-think (R1) ────────────────────────

#[tokio::test]
async fn router_on_curator_timeout_fallback_no_think() {
    let provider = MockServer::start().await;
    let curator = MockServer::start().await;
    mount_provider(&provider).await;
    // Curateur lent (400 ms) alors que le timeout routeur est à 100 ms → Timeout → fallback.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(400))
                .set_body_json(json!({
                    "choices": [{ "message": { "content": "THINK" } }]
                })),
        )
        .mount(&curator)
        .await;

    let status = post_chat(
        config_router_on(&provider.uri(), &curator.uri(), 100),
        "what port does example-dns use",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Le curateur a été TENTÉ (frontière) mais a expiré → fallback no-think observable.
    let curator_hits = curator.received_requests().await.unwrap_or_default();
    assert_eq!(curator_hits.len(), 1, "curateur tenté 1 fois avant timeout");

    let fwd = single_forwarded_body(&provider).await;
    assert_eq!(
        fwd.pointer("/chat_template_kwargs/enable_thinking")
            .and_then(Value::as_bool),
        Some(false),
        "fallback = no-think (enable_thinking false) malgré le THINK jamais lu"
    );
    assert_eq!(
        fwd.get("temperature").and_then(Value::as_f64),
        Some(0.4),
        "fallback → preset NoThink"
    );
}

// ── Cas 5 : /metrics expose les séries routeur après une décision fallback ──────

#[tokio::test]
async fn router_on_metrics_expose_series_routeur_apres_fallback() {
    let provider = MockServer::start().await;
    mount_provider(&provider).await;
    // Curateur injoignable (port fermé) → décision fallback (raison http).
    let config = config_router_on(&provider.uri(), "http://127.0.0.1:1", 200);
    let state = AppState::for_test(config);
    let app = build_router(state);

    // 1) requête chat frontière → routeur → fallback no-think (curateur down).
    let chat_body = json!({
        "model": "agent-main",
        "messages": [{"role": "user", "content": "what port does example-dns use"}]
    });
    let chat_req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(chat_body.to_string()))
        .unwrap();
    let chat_status = app.clone().oneshot(chat_req).await.unwrap().status();
    assert_eq!(chat_status, StatusCode::OK);

    // 2) GET /metrics sur le MÊME AppState (metrics partagées via Arc).
    let metrics_req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(metrics_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);

    // Les 4 séries D-9 (nom contenant router/fallback) doivent être présentes.
    assert!(
        text.contains("gateway_router_decisions_total{source=\"fallback\"} 1"),
        "décision fallback comptée : {text}"
    );
    assert!(
        text.contains("gateway_router_fallback_total{reason=\"http\"} 1"),
        "fallback raison http compté : {text}"
    );
    assert!(
        text.contains("gateway_router_curator_latency_seconds_bucket"),
        "histogramme latence curateur exposé"
    );
    assert!(
        text.contains("gateway_router_system_latency_seconds_bucket"),
        "histogramme latence système exposé"
    );
    assert!(
        text.contains("gateway_router_system_latency_seconds_count 1"),
        "1 décision système observée"
    );
}

// ── Cas 4 : override X-Reasoning-Mode → court-circuite le routeur ────────────────

#[tokio::test]
async fn router_on_override_header_court_circuite_le_routeur() {
    let provider = MockServer::start().await;
    let curator = MockServer::start().await;
    mount_provider(&provider).await;
    // Curateur répondrait NO_THINK — mais l'override doit gagner ET l'éviter entièrement.
    mount_curator(&curator, "NO_THINK").await;

    // "hello there" serait NoThink en pré-classif ; l'override force think.
    let status = post_chat(
        config_router_on(&provider.uri(), &curator.uri(), 500),
        "hello there",
        Some("think"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Override présent → le handler n'interroge PAS le routeur (économie de latence).
    let curator_hits = curator.received_requests().await.unwrap_or_default();
    assert_eq!(
        curator_hits.len(),
        0,
        "override court-circuite le routeur (0 appel curateur)"
    );

    let fwd = single_forwarded_body(&provider).await;
    assert_eq!(
        fwd.pointer("/chat_template_kwargs/enable_thinking")
            .and_then(Value::as_bool),
        Some(true),
        "override think → enable_thinking = true"
    );
}
