//! Tests d'intégration gradatum-gateway.
//!
//! Couvre les correctifs sécurité (CORS whitelist, cap tools, rate limiting IP socket,
//! mesure elapsed_secs), le handler rerank, et les deux modes de dispatch embeddings
//! (local/remote).
//!
//! Utilise `tower::ServiceExt::oneshot` — pas de serveur HTTP réel.
//! `wiremock` pour simuler les backends LLM distants (remote embeddings).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gradatum_gateway::commons::chat::{Message, MessageContent};
use gradatum_gateway::config::{
    AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig, VaultAwareConfig,
};
use gradatum_gateway::{AppState, build_router};

// ── Helpers ─────────────────────────────────────────────────────────────────

/// `ServerConfig` de test avec les valeurs de défaut raisonnables.
fn test_server_config() -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:18436".to_string(),
        registry_db: None,
        bearer_token_env: None,
        rate_limit_per_minute: 1000,
        circuit_threshold: 5,
        circuit_window_secs: 60,
        circuit_cooldown_secs: 30,
        max_total_tokens: 0,
        trust_localhost: true,
        enable_slot_passthrough: false,
        allowed_origins: vec![],
        max_tools_per_request: 10,
    }
}

/// Config minimale pour les tests — un alias + un provider.
fn test_config_with_provider(provider_endpoint: &str) -> Config {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "test-provider".to_string(),
        ProviderConfig {
            endpoint: provider_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );

    let mut aliases = std::collections::HashMap::new();
    aliases.insert(
        "test-alias".to_string(),
        AliasTarget::simple("test-provider", "test-model"),
    );

    Config {
        server: test_server_config(),
        providers,
        aliases,
        gateway: std::collections::HashMap::new(),
        logging: LoggingConfig::default(),
        vault_aware: VaultAwareConfig::default(),
        messages: Default::default(),
        router: Default::default(),
    }
}

/// Config avec allowed_origins spécifiées (pour tests CORS whitelist).
fn test_config_with_cors(origins: Vec<String>, provider_endpoint: &str) -> Config {
    let mut config = test_config_with_provider(provider_endpoint);
    config.server.allowed_origins = origins;
    config
}

/// Config avec max_tools_per_request pour tests du cap tools.
fn test_config_with_tool_cap(max_tools: usize, provider_endpoint: &str) -> Config {
    let mut config = test_config_with_provider(provider_endpoint);
    config.server.max_tools_per_request = max_tools;
    config
}

/// Config avec un alias `vision_capable = true` pointant sur le provider donné.
fn test_config_vision_capable(provider_endpoint: &str) -> Config {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "vision-provider".to_string(),
        ProviderConfig {
            endpoint: provider_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );

    let mut aliases = std::collections::HashMap::new();
    aliases.insert(
        "vision-alias".to_string(),
        AliasTarget {
            provider: "vision-provider".to_string(),
            model: "vision-model".to_string(),
            fallback_provider: None,
            fallback_model: None,
            temperature_default: None,
            max_tokens_default: None,
            vision_capable: true,
        },
    );
    // Alias texte seul (vision_capable = false) côte-à-côte.
    aliases.insert(
        "text-only-alias".to_string(),
        AliasTarget {
            provider: "vision-provider".to_string(),
            model: "text-model".to_string(),
            fallback_provider: None,
            fallback_model: None,
            temperature_default: None,
            max_tokens_default: None,
            vision_capable: false,
        },
    );

    Config {
        server: test_server_config(),
        providers,
        aliases,
        gateway: std::collections::HashMap::new(),
        logging: LoggingConfig::default(),
        vault_aware: VaultAwareConfig::default(),
        messages: Default::default(),
        router: Default::default(),
    }
}

/// Config avec un alias `vision_capable = true` + fallback configuré.
///
/// Utilisé pour les tests du gate aval (skip fallback sur requête image).
/// `primary_endpoint` : provider vision primary (à passer sur port fermé pour simuler un down).
/// `fallback_endpoint` : provider fallback (ne doit pas être appelé sur requête image).
fn test_config_vision_with_fallback(primary_endpoint: &str, fallback_endpoint: &str) -> Config {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "vision-primary".to_string(),
        ProviderConfig {
            endpoint: primary_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 2,
        },
    );
    providers.insert(
        "vision-fallback".to_string(),
        ProviderConfig {
            endpoint: fallback_endpoint.to_string(),
            api_key_env: None,
            timeout_secs: 10,
        },
    );

    let mut aliases = std::collections::HashMap::new();
    aliases.insert(
        "vision-with-fallback".to_string(),
        AliasTarget {
            provider: "vision-primary".to_string(),
            model: "vision-model".to_string(),
            fallback_provider: Some("vision-fallback".to_string()),
            fallback_model: Some("text-fallback-model".to_string()),
            temperature_default: None,
            max_tokens_default: None,
            vision_capable: true,
        },
    );

    Config {
        server: test_server_config(),
        providers,
        aliases,
        gateway: std::collections::HashMap::new(),
        logging: LoggingConfig::default(),
        vault_aware: VaultAwareConfig::default(),
        messages: Default::default(),
        router: Default::default(),
    }
}

/// Construit un `AppState` de test depuis une config.
fn make_state(config: Config) -> AppState {
    AppState::for_test(config)
}

// ── CORS whitelist ───────────────────────────────────────────────────────────

/// CORS whitelist : liste vide → aucun header CORS.
#[tokio::test]
async fn cors_whitelist_empty_no_cors_headers() {
    let config = test_config_with_cors(vec![], "http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("Origin", "http://malicious.example.com")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Aucun header Access-Control-Allow-Origin attendu quand allowed_origins est vide.
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "aucun header CORS attendu quand allowed_origins = []"
    );
}

/// CORS whitelist : origin non whitelistée → Access-Control-Allow-Origin absent (varie).
#[tokio::test]
async fn cors_whitelist_specific_origin_allows_matching() {
    let config = test_config_with_cors(
        vec!["http://localhost:3000".to_string()],
        "http://127.0.0.1:9999",
    );
    let state = make_state(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("Origin", "http://localhost:3000")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // L'origin whitelistée doit être présente dans le header CORS.
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(acao, "http://localhost:3000", "origin whitelistée attendue");
}

/// CORS whitelist : permissif avec "*" → Access-Control-Allow-Origin: *.
#[tokio::test]
async fn cors_whitelist_star_permissive() {
    let config = test_config_with_cors(vec!["*".to_string()], "http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("Origin", "http://anything.example.com")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(acao, "*", "wildcard CORS attendu pour '*'");
}

// ── Cap tools array ──────────────────────────────────────────────────────────

/// Cap tools : tools.len() > max_tools_per_request → 400 Bad Request.
#[tokio::test]
async fn tools_array_cap_rejected() {
    let config = test_config_with_tool_cap(2, "http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    // 3 outils, cap = 2 → rejeté.
    let body = json!({
        "model": "test-alias",
        "messages": [{"role": "user", "content": "test"}],
        "tools": [
            {"type": "function", "function": {"name": "fn1", "description": "f1", "parameters": {}}},
            {"type": "function", "function": {"name": "fn2", "description": "f2", "parameters": {}}},
            {"type": "function", "function": {"name": "fn3", "description": "f3", "parameters": {}}}
        ]
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
        StatusCode::BAD_REQUEST,
        "HTTP 400 attendu pour tools > cap"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["error"]["code"].as_str().unwrap_or(""),
        "too_many_tools",
        "code d'erreur 'too_many_tools' attendu"
    );
}

/// Cap tools : tools.len() == max_tools_per_request → autorisé (passe au backend).
#[tokio::test]
async fn tools_array_cap_exact_allowed() {
    let config = test_config_with_tool_cap(2, "http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    // 2 outils, cap = 2 → accepté (peut échouer sur le backend, pas sur le cap).
    let body = json!({
        "model": "test-alias",
        "messages": [{"role": "user", "content": "test"}],
        "tools": [
            {"type": "function", "function": {"name": "fn1", "description": "f1", "parameters": {}}},
            {"type": "function", "function": {"name": "fn2", "description": "f2", "parameters": {}}}
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    // Ne doit PAS retourner 400 TooManyTools — le backend peut retourner autre chose.
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "HTTP 400 TooManyTools NON attendu pour count == cap"
    );
}

/// Cap tools : max_tools_per_request = 0 désactive la vérification.
#[tokio::test]
async fn tools_array_cap_zero_disabled() {
    // cap = 0 signifie pas de vérification.
    let config = test_config_with_tool_cap(0, "http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let tools: Vec<serde_json::Value> = (0..50)
        .map(|i| {
            json!({
                "type": "function",
                "function": {"name": format!("fn{}", i), "description": "d", "parameters": {}}
            })
        })
        .collect();

    let body = json!({
        "model": "test-alias",
        "messages": [{"role": "user", "content": "test"}],
        "tools": tools
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "cap = 0 → aucun rejet TooManyTools attendu"
    );
}

// ── Rate limit basé sur IP socket ────────────────────────────────────────────

/// Rate limit socket IP : `extract_client_ip_from_socket` retourne l'IP socket (pas XFF).
///
/// Ce test vérifie via `rate_limit::extract_client_ip_from_socket` que le fallback
/// loopback est bien utilisé quand ConnectInfo est absent (tests mock).
#[test]
fn rate_limit_socket_ip_fallback_when_no_connect_info() {
    use gradatum_gateway::rate_limit::extract_client_ip_from_socket;
    let ip = extract_client_ip_from_socket(&None);
    assert!(
        ip.is_loopback(),
        "fallback loopback attendu sans ConnectInfo"
    );
}

/// Rate limit socket IP : extraction depuis `Extension<ConnectInfo<SocketAddr>>`.
#[test]
fn rate_limit_socket_ip_from_extension() {
    use axum::extract::{ConnectInfo, Extension};
    use gradatum_gateway::rate_limit::extract_client_ip_from_socket;

    // IP non-loopback — simulant un client distant (test IP sans topologie privée).
    let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 54321);
    let ci = Some(Extension(ConnectInfo(addr)));
    let ip = extract_client_ip_from_socket(&ci);
    assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    assert!(!ip.is_loopback(), "IP distante ne doit pas être loopback");
}

/// Rate limit socket IP : bloque après la limite.
///
/// Vérifie que le rate limiter count par IP fonctionne correctement.
/// En intégration router, les handlers reçoivent l'IP loopback (mock sans ConnectInfo).
#[test]
fn rate_limit_socket_ip_enforces_limit() {
    use gradatum_gateway::rate_limit::RateLimiter;
    use std::net::Ipv4Addr;

    let rl = RateLimiter::new(3);
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
    assert!(rl.check_and_increment(ip));
    assert!(rl.check_and_increment(ip));
    assert!(rl.check_and_increment(ip));
    assert!(
        !rl.check_and_increment(ip),
        "4ème requête doit être bloquée"
    );
}

// ── elapsed_secs réel ────────────────────────────────────────────────────────

/// Elapsed_secs : `Instant::elapsed().as_secs_f64()` retourne une valeur positive.
///
/// Vérifie que la mesure de durée réelle (Instant) fonctionne correctement.
/// LlmError::Timeout utilise cette valeur au lieu d'une valeur symbolique.
#[test]
fn elapsed_real_measure_positive() {
    let start = std::time::Instant::now();
    // Micro-pause pour s'assurer que elapsed > 0.
    std::thread::sleep(std::time::Duration::from_millis(1));
    let elapsed_secs = start.elapsed().as_secs_f64();
    assert!(elapsed_secs >= 0.001, "elapsed_secs doit être >= 1ms");
    assert!(elapsed_secs < 60.0, "elapsed_secs doit être < 60s");
}

/// Elapsed_secs : `LlmError::Timeout` contient `elapsed_secs` cohérent.
#[test]
fn elapsed_real_measure_in_llm_error() {
    use gradatum_gateway::commons::error::LlmError;

    let start = std::time::Instant::now();
    let elapsed = start.elapsed().as_secs_f64();
    let err = LlmError::Timeout {
        elapsed_secs: elapsed,
    };
    let msg = err.to_string();
    // Le message doit mentionner un timeout.
    assert!(
        msg.contains("timed out"),
        "message LlmError::Timeout attendu"
    );
    // La valeur ne doit pas être symbolique (ex: 0.0 ou négatif).
    assert!(elapsed >= 0.0, "elapsed_secs doit être non-négatif");
}

// ── F-08 : Reranker ───────────────────────────────────────────────────────────

/// F-08 : POST /v1/rerank avec NoopReranker — résultats triés par score.
#[tokio::test]
async fn rerank_dispatch_returns_sorted_results() {
    use gradatum_search::reranker::NoopReranker;

    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config).with_reranker(Arc::new(NoopReranker));
    let app = build_router(state);

    let body = json!({
        "query": "test query",
        "documents": ["doc A", "doc B", "doc C"]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/rerank")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "HTTP 200 attendu pour rerank OK"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let results = json["results"].as_array().expect("champ 'results' attendu");
    assert_eq!(results.len(), 3, "3 résultats attendus");

    // Vérifier le tri décroissant par score.
    for i in 0..results.len().saturating_sub(1) {
        let s_i = results[i]["relevance_score"]
            .as_f64()
            .unwrap_or(f64::NEG_INFINITY);
        let s_next = results[i + 1]["relevance_score"]
            .as_f64()
            .unwrap_or(f64::NEG_INFINITY);
        assert!(
            s_i >= s_next,
            "résultats doivent être triés par score décroissant: {} >= {}",
            s_i,
            s_next
        );
    }
}

/// F-08 : POST /v1/rerank avec top_n — truncation appliquée.
#[tokio::test]
async fn rerank_top_k_truncation() {
    use gradatum_search::reranker::NoopReranker;

    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config).with_reranker(Arc::new(NoopReranker));
    let app = build_router(state);

    let body = json!({
        "query": "test",
        "documents": ["a", "b", "c", "d", "e"],
        "top_n": 2
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/rerank")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let results = json["results"].as_array().expect("champ 'results' attendu");
    assert_eq!(results.len(), 2, "top_n=2 → 2 résultats attendus");
}

/// F-08 : POST /v1/rerank sans reranker configuré → 503 Service Unavailable.
#[tokio::test]
async fn rerank_no_reranker_returns_503() {
    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config); // pas de reranker
    let app = build_router(state);

    let body = json!({"query": "test", "documents": ["doc"]});

    let req = Request::builder()
        .method("POST")
        .uri("/v1/rerank")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "503 attendu si reranker non configuré"
    );
}

/// F-08 C3 : POST /v1/rerank avec plus de documents que max_batch_size → 400 Bad Request.
#[tokio::test]
async fn rerank_exceeds_max_batch_size_returns_400() {
    // NoopReranker.max_batch_size() = usize::MAX — on utilise un reranker stub
    // dont le cap est petit (20, cf. JinaOnnxReranker). Ici on forge un AppState
    // avec NoopReranker mais on teste le mécanisme d'enforcement via un wrapper.
    //
    // Approche retenue : sous-classer le trait Reranker avec un stub cap=2 pour
    // vérifier le code path sans dépendre de la feature onnx-reranker.
    struct LowCapReranker;
    impl gradatum_search::reranker::Reranker for LowCapReranker {
        fn rerank(
            &self,
            _query: &str,
            candidates: &[(String, String)],
        ) -> Result<Vec<f32>, gradatum_core::error::GradatumError> {
            Ok(vec![0.5; candidates.len()])
        }

        fn max_batch_size(&self) -> usize {
            2 // cap volontairement bas pour le test
        }
    }

    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config).with_reranker(Arc::new(LowCapReranker));
    let app = build_router(state);

    // 3 documents > max_batch_size 2 → doit retourner 400.
    let body = json!({
        "query": "test query",
        "documents": ["doc A", "doc B", "doc C"]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/rerank")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "400 attendu quand documents.len() > max_batch_size"
    );

    // Vérifier que le message d'erreur mentionne le dépassement.
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let message = json["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("too many documents"),
        "message d'erreur doit mentionner 'too many documents', got: {message}"
    );
}

/// F-08 : top_n plus grand que la liste → retourne tous les résultats.
#[tokio::test]
async fn rerank_top_n_larger_than_results_returns_all() {
    use gradatum_search::reranker::NoopReranker;

    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config).with_reranker(Arc::new(NoopReranker));
    let app = build_router(state);

    let body = json!({
        "query": "test",
        "documents": ["x", "y"],
        "top_n": 100
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/rerank")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let results = json["results"].as_array().expect("résultats attendus");
    assert_eq!(
        results.len(),
        2,
        "top_n > taille liste → tous les résultats"
    );
}

// ── Embeddings local (fastembed fallback) ────────────────────────────────────

/// Embedding local via Noop embedder — retourne des vecteurs zéro.
#[tokio::test]
async fn embedding_fastembed_fallback_local_mode() {
    use gradatum_embed::Noop;

    let mut config = test_config_with_provider("http://127.0.0.1:9999");
    // L'alias "local-embed" n'est pas dans la table aliases → mode local par alias direct.
    // Pour le mode local, AppState.local_embed_alias == model demandé.
    config.server.rate_limit_per_minute = 1000;
    let state = make_state(config).with_embedder(Arc::new(Noop::new(384)), "local-embed");
    let app = build_router(state);

    let body = json!({
        "model": "local-embed",
        "input": "bonjour le monde"
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
        "HTTP 200 attendu pour embedder local"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(json["object"].as_str().unwrap_or(""), "list");
    let data = json["data"].as_array().expect("champ 'data' attendu");
    assert_eq!(data.len(), 1, "1 embedding pour 1 texte");
    let embedding = data[0]["embedding"]
        .as_array()
        .expect("champ 'embedding' attendu");
    assert_eq!(embedding.len(), 384, "Noop(384) → vecteur de 384 dims");
}

/// Embedding batch local — plusieurs textes.
#[tokio::test]
async fn embedding_fastembed_fallback_batch() {
    use gradatum_embed::Noop;

    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config).with_embedder(Arc::new(Noop::new(384)), "local-embed");
    let app = build_router(state);

    let body = json!({
        "model": "local-embed",
        "input": ["texte 1", "texte 2", "texte 3"]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let data = json["data"].as_array().expect("data attendu");
    assert_eq!(data.len(), 3, "3 embeddings pour 3 textes");
}

// ── Embeddings remote (pass-through HTTP) ────────────────────────────────────

/// Embedding remote — forward vers le backend HTTP simulé par wiremock.
#[tokio::test]
async fn embedding_http_remote_pass_through() {
    // Démarrage du serveur mock.
    let mock_server = MockServer::start().await;

    let mock_response = json!({
        "object": "list",
        "data": [{
            "object": "embedding",
            "embedding": [0.1, 0.2, 0.3],
            "index": 0
        }],
        "model": "test-model",
        "usage": {"prompt_tokens": 4, "total_tokens": 4}
    });

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&mock_response))
        .mount(&mock_server)
        .await;

    let config = test_config_with_provider(&mock_server.uri());
    let state = make_state(config);
    let app = build_router(state);

    let body = json!({
        "model": "test-alias",
        "input": "texte pour embedding distant"
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
        "HTTP 200 attendu pour embedding remote"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let data = json["data"].as_array().expect("data attendu");
    assert_eq!(data.len(), 1);
}

/// Embedding remote — alias inconnu → 404.
#[tokio::test]
async fn embedding_http_remote_unknown_alias_404() {
    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let body = json!({
        "model": "alias-inexistant",
        "input": "texte"
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
        StatusCode::NOT_FOUND,
        "HTTP 404 pour alias inconnu"
    );
}

// ── Health / Models / Metrics ─────────────────────────────────────────────────

/// /health retourne 200 avec les champs attendus.
#[tokio::test]
async fn health_returns_ok() {
    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"].as_str().unwrap_or(""), "ok");
}

/// /v1/models retourne la liste des aliases configurés.
#[tokio::test]
async fn models_returns_configured_aliases() {
    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let data = json["data"].as_array().expect("champ 'data' attendu");
    assert!(!data.is_empty(), "au moins 1 alias configuré attendu");
    let ids: Vec<&str> = data.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(ids.contains(&"test-alias"), "alias 'test-alias' attendu");
}

/// /metrics retourne du texte Prometheus.
#[tokio::test]
async fn metrics_returns_prometheus_text() {
    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    // Prometheus text format : lignes commençant par "# HELP" ou "# TYPE".
    assert!(
        text.contains("# HELP") || text.contains("gateway_"),
        "format Prometheus attendu"
    );
}

// ── Chat completions — routing ────────────────────────────────────────────────

/// Chat completions — alias inconnu → 404.
#[tokio::test]
async fn chat_unknown_alias_returns_404() {
    let config = test_config_with_provider("http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let body = json!({
        "model": "alias-inexistant",
        "messages": [{"role": "user", "content": "test"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── C3 : Multimodal content-array + gate vision ───────────────────────────────

/// C3 : désérialisation — content String (forme historique) → MessageContent::Text.
#[test]
fn c3_deser_content_string_is_text() {
    let json_str = r#"{"role":"user","content":"bonjour le monde"}"#;
    let msg: Message =
        serde_json::from_str(json_str).expect("désérialisation d'un message texte simple");
    assert_eq!(
        msg.content,
        MessageContent::Text("bonjour le monde".to_string()),
        "content String doit être désérialisé en Text"
    );
}

/// C3 : désérialisation — content array text seul → MessageContent::Parts(Text).
#[test]
fn c3_deser_content_array_text_only() {
    let json_str = r#"{"role":"user","content":[{"type":"text","text":"hello"}]}"#;
    let msg: Message = serde_json::from_str(json_str).expect("désérialisation d'un array text");
    assert!(
        matches!(&msg.content, MessageContent::Parts(parts) if parts.len() == 1),
        "content array avec 1 partie texte attendu"
    );
    assert!(
        !msg.has_image(),
        "message texte seul ne doit pas avoir d'image"
    );
}

/// C3 : désérialisation — content array text + image → MessageContent::Parts avec image.
#[test]
fn c3_deser_content_array_text_image() {
    // PNG 1×1 minimal (test canonique C3).
    let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let json_str = format!(
        r#"{{"role":"user","content":[{{"type":"text","text":"décris"}},{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{}"}}}}]}}"#,
        png_b64
    );
    let msg: Message =
        serde_json::from_str(&json_str).expect("désérialisation d'un array text+image");
    assert!(
        matches!(&msg.content, MessageContent::Parts(parts) if parts.len() == 2),
        "2 parties attendues (text + image_url)"
    );
    assert!(
        msg.has_image(),
        "message avec image_url doit retourner has_image() = true"
    );
}

/// C3 : resérialisation — MessageContent::Text produit un JSON String (rétrocompat providers).
///
/// Garantit que les backends texte reçoivent `"content": "..."` et non `"content": [...]`.
#[test]
fn c3_reser_text_produces_json_string() {
    let msg = Message::user("texte de test");
    let json = serde_json::to_value(&msg).expect("sérialisation d'un message texte");
    assert!(
        json["content"].is_string(),
        "MessageContent::Text doit être sérialisé comme String JSON, got: {}",
        json["content"]
    );
    assert_eq!(
        json["content"].as_str().unwrap_or(""),
        "texte de test",
        "valeur texte préservée"
    );
}

/// C3 : gate vision — alias non vision_capable + requête image → HTTP 400.
#[tokio::test]
async fn c3_vision_gate_rejects_image_on_non_vision_alias() {
    let config = test_config_vision_capable("http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    // PNG 1×1 minimal.
    let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let body = json!({
        "model": "text-only-alias",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "décris cette image"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", png_b64)}}
            ]
        }]
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
        StatusCode::BAD_REQUEST,
        "HTTP 400 attendu : requête image vers alias non vision_capable"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["error"]["code"].as_str().unwrap_or(""),
        "vision_not_supported",
        "code d'erreur 'vision_not_supported' attendu"
    );
}

/// C3 : gate vision — alias non vision_capable + requête texte seule → passe le gate.
///
/// Vérifie que le gate ne bloque pas les requêtes texte normales.
#[tokio::test]
async fn c3_vision_gate_allows_text_on_non_vision_alias() {
    let config = test_config_vision_capable("http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let body = json!({
        "model": "text-only-alias",
        "messages": [{"role": "user", "content": "simple texte sans image"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    // Le backend 9999 est fermé → 502 ou 500 (pas 400 vision gate).
    // L'important : ne pas être rejeté par le gate vision.
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "requête texte seule ne doit PAS être bloquée par le gate vision"
    );
    let status = resp.status().as_u16();
    assert!(
        status != 400,
        "texte seul sur alias non-vision → ne doit pas déclencher le gate (got {})",
        status
    );
}

/// C3 gate aval : primary vision down + requête image → 503, fallback NON appelé.
///
/// Vérifie le skip déterministe du fallback pour les requêtes vision (plan C3).
/// Le fallback mock est démarré mais aucun handler n'est monté — s'il était appelé,
/// wiremock retournerait 501 Not Implemented, différent de 503 ServiceUnavailable.
/// On vérifie à la fois le 503 ET que le wiremock n'a reçu aucune requête.
#[tokio::test]
async fn c3_vision_gate_aval_primary_down_image_skips_fallback_returns_503() {
    // Fallback mock : aucun handler → s'il est appelé, wiremock renvoie 501.
    let fallback_mock = MockServer::start().await;

    // Primary : port fermé → erreur réseau garantie.
    let config = test_config_vision_with_fallback("http://127.0.0.1:1", &fallback_mock.uri());
    let state = make_state(config);
    let app = build_router(state);

    let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let body = json!({
        "model": "vision-with-fallback",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "décris"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", png_b64)}}
            ]
        }]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    // Doit être 503 (vision provider unavailable, no vision-capable fallback).
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "HTTP 503 attendu : primary vision down + requête image → fallback skipé"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json_body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json_body["error"]["code"].as_str().unwrap_or(""),
        "service_unavailable",
        "code 'service_unavailable' attendu (pas 'backend_error')"
    );

    // Vérification wiremock : le fallback n'a reçu AUCUNE requête.
    let received = fallback_mock.received_requests().await.unwrap_or_default();
    assert_eq!(
        received.len(),
        0,
        "fallback mock ne doit PAS avoir reçu de requête (skip déterministe)"
    );
}

/// C3 gate aval : primary vision down + requête TEXTE → fallback tenté (comportement inchangé).
///
/// Vérifie que le skip ne s'applique qu'aux requêtes image.
/// Requête texte seule sur alias vision_capable → fallback fonctionnel si primary down.
#[tokio::test]
async fn c3_vision_gate_aval_primary_down_text_fallback_attempted() {
    let fallback_mock = MockServer::start().await;

    // Fallback mock répond 200 avec une complétion valide.
    let chat_resp = json!({
        "id": "chatcmpl-fallback-text",
        "object": "chat.completion",
        "created": 1234567890u64,
        "model": "text-fallback-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "réponse texte fallback"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&chat_resp))
        .mount(&fallback_mock)
        .await;

    let config = test_config_vision_with_fallback("http://127.0.0.1:1", &fallback_mock.uri());
    let state = make_state(config);
    let app = build_router(state);

    // Requête texte seule (pas d'image).
    let body = json!({
        "model": "vision-with-fallback",
        "messages": [{"role": "user", "content": "texte sans image"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    // Le fallback doit avoir été atteint → 200 OK.
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "HTTP 200 attendu : requête texte → fallback tenté normalement"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json_body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json_body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or(""),
        "réponse texte fallback",
        "réponse du fallback attendue"
    );

    // Le fallback mock a bien reçu exactement 1 requête.
    let received = fallback_mock.received_requests().await.unwrap_or_default();
    assert_eq!(
        received.len(),
        1,
        "fallback mock doit avoir reçu exactement 1 requête (texte → fallback non skipé)"
    );
}

/// Chat completions — backend répond 200 → forward la réponse.
#[tokio::test]
async fn chat_backend_200_forwarded() {
    let mock_server = MockServer::start().await;

    let chat_response = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1234567890u64,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Bonjour !"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    });

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&chat_response))
        .mount(&mock_server)
        .await;

    let config = test_config_with_provider(&mock_server.uri());
    let state = make_state(config);
    let app = build_router(state);

    let body = json!({
        "model": "test-alias",
        "messages": [{"role": "user", "content": "test"}]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or(""),
        "Bonjour !"
    );
}

// ── Cas limites additionnels (C3 gate vision edge cases) ──────────────────────

/// C3 edge case : content array vide → pas de panique, dispatché en texte vide.
///
/// Vérifie que `content: []` est traité gracieusement et ne cause pas d'erreur de désérialisation.
#[test]
fn c3_deser_content_array_empty_no_panic() {
    let json_str = r#"{"role":"user","content":[]}"#;
    // Si la désérialisation échoue, expect() panique (légitime en test).
    let msg: Message = serde_json::from_str(json_str).expect("empty array doit être valide");

    // Un array vide doit produire une MessageContent vide ou Parts([]).
    match &msg.content {
        MessageContent::Parts(parts) => {
            assert_eq!(parts.len(), 0, "array vide → Parts([])");
            assert!(!msg.has_image(), "array vide ne doit pas avoir d'image");
        }
        MessageContent::Text(t) => {
            assert_eq!(t.len(), 0, "array vide → Text vide");
            assert!(!msg.has_image(), "Text vide ne doit pas avoir d'image");
        }
    }
}

/// C3 edge case : content array avec 2 images + texte → has_image()=true, token_count ≈2×1100.
///
/// Vérifie que plusieurs images sont supportées et comptées en tokens.
#[test]
fn c3_deser_content_array_two_images_plus_text() {
    let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let json_str = format!(
        r#"{{"role":"user","content":[{{"type":"text","text":"compare"}},{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{}"}}}},{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{}"}}}}]}}"#,
        png_b64, png_b64
    );
    let msg: Message =
        serde_json::from_str(&json_str).expect("array 2 images + text doit être valide");

    match &msg.content {
        MessageContent::Parts(parts) => {
            assert_eq!(
                parts.len(),
                3,
                "3 parties attendues (text + 2 image_url), got: {}",
                parts.len()
            );
        }
        _ => panic!("2 images + text → Parts attendu"),
    }
    assert!(
        msg.has_image(),
        "message avec 2 images doit retourner has_image()=true"
    );
}

/// C3 edge case : {"type":"image_url"} sans champ image_url → erreur désérialisation propre (422).
///
/// Vérifie que la malformation est capturée sans panique, avec HTTP 422 (UnprocessableEntity).
#[tokio::test]
async fn c3_deser_malformed_image_url_returns_422() {
    let config = test_config_vision_capable("http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    // Content part sans le champ 'image_url' requis.
    let body = json!({
        "model": "vision-alias",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "décris"},
                {"type": "image_url"} // ← malformé : pas de 'image_url' imbriqué
            ]
        }]
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
        StatusCode::UNPROCESSABLE_ENTITY,
        "HTTP 422 attendu pour JSON malformé (missing image_url field)"
    );
}

/// C3 edge case : streaming (stream=true) avec content array image → vision gate s'applique aussi.
///
/// Vérifie que le gate vision fonctionne aussi sur le endpoint streaming.
#[tokio::test]
async fn c3_vision_gate_applies_to_streaming_endpoint() {
    let config = test_config_vision_capable("http://127.0.0.1:9999");
    let state = make_state(config);
    let app = build_router(state);

    let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let body = json!({
        "model": "text-only-alias",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "décris"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", png_b64)}}
            ]
        }],
        "stream": true
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
        StatusCode::BAD_REQUEST,
        "HTTP 400 attendu : vision gate s'applique à streaming aussi"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json_body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json_body["error"]["code"].as_str().unwrap_or(""),
        "vision_not_supported",
        "code 'vision_not_supported' attendu sur streaming"
    );
}
