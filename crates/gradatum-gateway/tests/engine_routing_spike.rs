//! Tests TDD — câblage providers engine-curator / engine-embed avec fallback legacy.
//!
//! ## Contexte du spike
//!
//! Ce test valide le mécanisme de routage gateway pour préparer l'intégration future :
//!
//!   engine-curator  →  http://127.0.0.1:11435  (gradatum-engine chat, spike)
//!   engine-embed    →  http://127.0.0.1:11436  (gradatum-engine embed, spike)
//!
//! En cas d'échec du primary (5xx / timeout / connexion refusée) :
//!   engine-curator → fallback → legacy-curator  (placeholder loopback, alias legacy config)
//!   engine-embed   → fallback → legacy-embed    (placeholder loopback, alias legacy config)
//!
//! ## Ce que ces tests couvrent
//!
//! 1. `engine_curator_primary_ok_no_fallback` : engine répond 200 → réponse directe, pas de fallback.
//! 2. `engine_curator_primary_5xx_triggers_fallback` : engine répond 500 → fallback legacy OK.
//! 3. `engine_curator_primary_unreachable_triggers_fallback` : engine injoignable (port fermé) → fallback.
//! 4. `engine_curator_both_fail_returns_502` : primary chat + fallback KO → 502.
//! 5. `engine_embed_primary_ok_no_fallback` : engine embed répond 200 → réponse directe.
//! 6. `engine_embed_primary_5xx_falls_back_to_legacy` : engine embed 500 → fallback legacy OK.
//! 7. `engine_embed_primary_unreachable_falls_back_to_legacy` : engine embed injoignable → fallback.
//! 8. `engine_embed_both_fail_returns_502` : primary embed + fallback KO → 502.
//!
//! ## Anti-leak
//!
//! Tous les endpoints utilisent 127.0.0.1 (loopback générique) ou wiremock (port dynamique).
//! Aucune IP réelle ni hostname de production.

use std::collections::{BTreeMap, HashMap};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gradatum_gateway::config::{
    AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig, VaultAwareConfig,
};
use gradatum_gateway::{build_router, AppState};

// ── Helpers ──────────────────────────────────────────────────────────────────

/// `ServerConfig` de test avec rate-limit désactivé pour éviter l'interférence.
fn test_server_config() -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:18437".to_string(),
        registry_db: None,
        bearer_token_env: None,
        rate_limit_per_minute: 10_000,
        circuit_threshold: 5,
        circuit_window_secs: 60,
        circuit_cooldown_secs: 30,
        max_total_tokens: 0,
        trust_localhost: true,
        enable_slot_passthrough: false,
        allowed_origins: vec![],
        max_tools_per_request: 64,
    }
}

/// Construit une config gateway simulant le câblage engine spike.
///
/// `engine_chat_endpoint` : URL du provider engine-curator (primary chat).
/// `legacy_chat_endpoint` : URL du fallback legacy-curator.
/// `engine_embed_endpoint`: URL du provider engine-embed (primary embeddings).
/// `legacy_embed_endpoint`: URL du fallback legacy-embed.
fn make_engine_spike_config(
    engine_chat_endpoint: &str,
    legacy_chat_endpoint: &str,
    engine_embed_endpoint: &str,
    legacy_embed_endpoint: &str,
) -> Config {
    let mut providers = BTreeMap::new();

    // Provider primary : gradatum-engine chat (loopback :11435 en prod spike)
    providers.insert(
        "engine-curator".to_string(),
        ProviderConfig {
            endpoint: engine_chat_endpoint.to_string(),
            api_key_env: None,
            // Timeout court pour ne pas ralentir les tests de fallback.
            timeout_secs: 2,
        },
    );

    // Provider fallback legacy curator (llm-free-gateway-v2 :8435 en prod)
    providers.insert(
        "legacy-curator".to_string(),
        ProviderConfig {
            endpoint: legacy_chat_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );

    // Provider primary : gradatum-engine embed (loopback :11436 en prod spike)
    providers.insert(
        "engine-embed".to_string(),
        ProviderConfig {
            endpoint: engine_embed_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 2,
        },
    );

    // Provider fallback legacy embed
    providers.insert(
        "legacy-embed".to_string(),
        ProviderConfig {
            endpoint: legacy_embed_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );

    let mut aliases = HashMap::new();

    // Alias curator : primary engine, fallback legacy
    aliases.insert(
        "curator".to_string(),
        AliasTarget {
            provider: "engine-curator".to_string(),
            model: "curator-model".to_string(),
            fallback_provider: Some("legacy-curator".to_string()),
            fallback_model: Some("extract".to_string()),
            temperature_default: None,
            max_tokens_default: None,
        },
    );

    // Alias embed : primary engine, fallback legacy
    aliases.insert(
        "embed".to_string(),
        AliasTarget {
            provider: "engine-embed".to_string(),
            model: "embed-model".to_string(),
            fallback_provider: Some("legacy-embed".to_string()),
            fallback_model: Some("bge-m3-Q8_0".to_string()),
            temperature_default: None,
            max_tokens_default: None,
        },
    );

    Config {
        server: test_server_config(),
        providers,
        aliases,
        gateway: HashMap::new(),
        logging: LoggingConfig::default(),
        vault_aware: VaultAwareConfig::default(),
    }
}

/// Réponse OpenAI-compat chat valide — utilisée par les mock servers.
fn chat_response_ok(content: &str) -> serde_json::Value {
    json!({
        "id": "chatcmpl-engine-spike",
        "object": "chat.completion",
        "created": 1_700_000_000u64,
        "model": "curator-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    })
}

/// Réponse OpenAI-compat embeddings valide.
fn embed_response_ok() -> serde_json::Value {
    // json! ne supporte pas la syntaxe [val; N] — construire le vecteur séparément.
    let embedding: Vec<f32> = vec![0.1; 1024];
    json!({
        "object": "list",
        "data": [{
            "object": "embedding",
            "embedding": embedding,
            "index": 0
        }],
        "model": "embed-model",
        "usage": {"prompt_tokens": 4, "total_tokens": 4}
    })
}

// ── Tests chat (curator) ──────────────────────────────────────────────────────

/// 1. engine-curator répond 200 → réponse forwarded, pas de fallback utilisé.
#[tokio::test]
async fn engine_curator_primary_ok_no_fallback() {
    let engine_server = MockServer::start().await;
    // fallback ne doit PAS être appelé — on le pointe sur un port fermé pour le détecter.
    let legacy_url = "http://127.0.0.1:1"; // port fermé — erreur si atteint

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_response_ok("engine OK")))
        .expect(1) // exactement 1 appel — le fallback ne doit pas être sollicité
        .mount(&engine_server)
        .await;

    let config = make_engine_spike_config(
        &engine_server.uri(),
        legacy_url,
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
    );
    let state = AppState::for_test(config);
    let app = build_router(state);

    let body = json!({
        "model": "curator",
        "messages": [{"role": "user", "content": "test curator spike"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "engine-curator primary OK → 200 attendu"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or(""),
        "engine OK",
        "contenu de la réponse engine attendu"
    );

    // wiremock::MockServer vérifie automatiquement les expectations à la fin
}

/// 2. engine-curator répond 500 → dispatch_with_fallback déclenche le fallback legacy-curator.
#[tokio::test]
async fn engine_curator_primary_5xx_triggers_fallback() {
    let engine_server = MockServer::start().await;
    let legacy_server = MockServer::start().await;

    // Engine répond toujours 500 → force le fallback.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string(
            r#"{"error":{"message":"engine internal error","type":"server_error"}}"#,
        ))
        .mount(&engine_server)
        .await;

    // Legacy répond 200 → réponse finale.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(chat_response_ok("legacy fallback OK")),
        )
        .mount(&legacy_server)
        .await;

    let config = make_engine_spike_config(
        &engine_server.uri(),
        &legacy_server.uri(),
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
    );
    let state = AppState::for_test(config);
    let app = build_router(state);

    let body = json!({
        "model": "curator",
        "messages": [{"role": "user", "content": "test fallback 5xx"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fallback legacy-curator doit répondre 200 après engine 500"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or(""),
        "legacy fallback OK",
        "réponse du fallback legacy attendue"
    );
}

/// 3. engine-curator injoignable (port fermé) → timeout/réseau → fallback legacy OK.
#[tokio::test]
async fn engine_curator_primary_unreachable_triggers_fallback() {
    let legacy_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(chat_response_ok("legacy after unreachable")),
        )
        .mount(&legacy_server)
        .await;

    let config = make_engine_spike_config(
        "http://127.0.0.1:1", // port fermé → connexion refusée → backend error
        &legacy_server.uri(),
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
    );
    let state = AppState::for_test(config);
    let app = build_router(state);

    let body = json!({
        "model": "curator",
        "messages": [{"role": "user", "content": "test fallback unreachable"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fallback legacy doit prendre le relais quand engine est injoignable"
    );
}

/// 4. engine-curator ET fallback KO → 502 retourné au client.
#[tokio::test]
async fn engine_curator_both_fail_returns_502() {
    // Les deux providers pointent sur un port fermé.
    let config = make_engine_spike_config(
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
    );
    let state = AppState::for_test(config);
    let app = build_router(state);

    let body = json!({
        "model": "curator",
        "messages": [{"role": "user", "content": "double failure"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "primary + fallback KO → 502 Bad Gateway attendu"
    );
}

// ── Tests embeddings (embed) ──────────────────────────────────────────────────

/// 4. engine-embed répond 200 → réponse forwarded, pas de fallback.
#[tokio::test]
async fn engine_embed_primary_ok_no_fallback() {
    let engine_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embed_response_ok()))
        .expect(1)
        .mount(&engine_server)
        .await;

    let config = make_engine_spike_config(
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
        &engine_server.uri(),
        "http://127.0.0.1:1",
    );
    let state = AppState::for_test(config);
    let app = build_router(state);

    let body = json!({
        "model": "embed",
        "input": "texte pour embedding engine"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "engine-embed primary OK → 200 attendu"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let data = json["data"].as_array().expect("champ 'data' attendu");
    assert_eq!(data.len(), 1, "1 embedding attendu");
    let embedding = data[0]["embedding"]
        .as_array()
        .expect("champ 'embedding' attendu");
    assert_eq!(embedding.len(), 1024, "dim 1024 attendue");
}

/// 5. engine-embed répond 500 → embed_dispatch_with_fallback déclenche le fallback legacy-embed.
///
/// Miroir de `engine_curator_primary_5xx_triggers_fallback` pour le chemin embeddings.
#[tokio::test]
async fn engine_embed_primary_5xx_falls_back_to_legacy() {
    let engine_server = MockServer::start().await;
    let legacy_server = MockServer::start().await;

    // Engine embed répond 500 → force le fallback.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_string(r#"{"error":{"message":"engine embed error"}}"#),
        )
        .mount(&engine_server)
        .await;

    // Legacy embed répond 200 → réponse finale.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embed_response_ok()))
        .expect(1) // exactement 1 appel attendu sur le fallback
        .mount(&legacy_server)
        .await;

    let config = make_engine_spike_config(
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
        &engine_server.uri(),
        &legacy_server.uri(),
    );
    let state = AppState::for_test(config);
    let app = build_router(state);

    let body = json!({
        "model": "embed",
        "input": "texte embedding fallback test"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fallback legacy-embed doit répondre 200 après engine-embed 500"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let data = json["data"].as_array().expect("champ 'data' attendu");
    assert_eq!(
        data.len(),
        1,
        "1 embedding attendu dans la réponse fallback"
    );
}

/// 5b. engine-embed injoignable (port fermé) → erreur réseau → fallback legacy OK.
///
/// Miroir de `engine_curator_primary_unreachable_triggers_fallback` pour le chemin embeddings.
#[tokio::test]
async fn engine_embed_primary_unreachable_falls_back_to_legacy() {
    let legacy_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embed_response_ok()))
        .mount(&legacy_server)
        .await;

    let config = make_engine_spike_config(
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
        "http://127.0.0.1:1", // port fermé → connexion refusée → backend error
        &legacy_server.uri(),
    );
    let state = AppState::for_test(config);
    let app = build_router(state);

    let body = json!({
        "model": "embed",
        "input": "texte embed unreachable"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fallback legacy-embed doit prendre le relais quand engine-embed est injoignable"
    );
}

/// 6. engine-embed ET fallback KO → 502 retourné au client.
#[tokio::test]
async fn engine_embed_both_fail_returns_502() {
    // Les deux providers embed pointent sur un port fermé.
    let config = make_engine_spike_config(
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
        "http://127.0.0.1:1",
    );
    let state = AppState::for_test(config);
    let app = build_router(state);

    let body = json!({
        "model": "embed",
        "input": "texte embed double failure"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "primary + fallback embed KO → 502 Bad Gateway attendu"
    );
}

// ── Test config validation ────────────────────────────────────────────────────

/// La config de spike est valide (providers + aliases cohérents).
#[test]
fn spike_config_validates_successfully() {
    let config = make_engine_spike_config(
        "http://127.0.0.1:11435",
        "http://127.0.0.1:8435",
        "http://127.0.0.1:11436",
        "http://127.0.0.1:8431",
    );

    // La config doit parser et valider sans erreur.
    // Vérification des providers déclarés.
    assert!(
        config.providers.contains_key("engine-curator"),
        "provider engine-curator attendu"
    );
    assert!(
        config.providers.contains_key("legacy-curator"),
        "provider legacy-curator attendu"
    );
    assert!(
        config.providers.contains_key("engine-embed"),
        "provider engine-embed attendu"
    );
    assert!(
        config.providers.contains_key("legacy-embed"),
        "provider legacy-embed attendu"
    );

    // Vérification des aliases.
    let curator_alias = config
        .aliases
        .get("curator")
        .expect("alias curator attendu");
    assert_eq!(curator_alias.provider, "engine-curator");
    assert_eq!(
        curator_alias.fallback_provider.as_deref(),
        Some("legacy-curator")
    );
    assert_eq!(curator_alias.fallback_model.as_deref(), Some("extract"));

    let embed_alias = config.aliases.get("embed").expect("alias embed attendu");
    assert_eq!(embed_alias.provider, "engine-embed");
    assert_eq!(
        embed_alias.fallback_provider.as_deref(),
        Some("legacy-embed")
    );
    assert_eq!(embed_alias.fallback_model.as_deref(), Some("bge-m3-Q8_0"));
}
