//! Serveur axum `gradatum-engine` — routes OpenAI-compat + `/health` + `/metrics`.
//!
//! ## Routes — port principal (bind_addr configurable)
//!
//! | Méthode | Path | Auth | Description |
//! |---------|------|------|-------------|
//! | GET | `/health` | non | État de warm-up + backend compilé |
//! | POST | `/v1/chat/completions` | non | Génération chat (proxy transparent) |
//! | POST | `/v1/embeddings` | non | Embedding (proxy transparent) |
//!
//! ## Routes — port metrics (loopback-only, 127.0.0.1:metrics_port — C2)
//!
//! | Méthode | Path | Auth | Description |
//! |---------|------|------|-------------|
//! | GET | `/metrics` | non | Métriques Prometheus text format |
//!
//! `/metrics` est intentionnellement absent du port principal. En configuration LAN
//! (bind_addr = IP LAN), les métriques ne doivent jamais être accessibles hors loopback.
//! Le listener metrics est géré par le binaire sur `127.0.0.1:metrics_port`.
//!
//! ## Sécurité
//!
//! - `bind_addr` fail-closed (C1) : validé dans `EngineConfig::validate()`.
//! - `DefaultBodyLimit` configurable via `body_limit_bytes` (défaut 32 MiB vision).
//! - Timeout via `tokio::time::timeout` → 504 (P1-2).
//!
//! ## Vague-2 — Reverse-proxy transparent
//!
//! Le backend typé `GenBackend` / `ProxyBackend` est supprimé — `ForwardProxy` (reqwest)
//! forwarde le body brut de la requête vers `llama-server`. Cela préserve automatiquement
//! `slot_id`, le sampling, les tools, la vision, `seed`, `response_format`, et le streaming
//! SSE. Le plan de contrôle/ops (superviseur, health, metrics, bind, env_clear) est inchangé.
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use gradatum_core::event_sink::{EngineEvent, EventSink};

use crate::{health::HealthState, metrics::EngineMetrics, runtime::ForwardProxy};

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// État partagé injecté dans les handlers axum.
///
/// ## Vague-2 — reverse-proxy transparent
///
/// `proxy` est un `ForwardProxy` (reqwest) qui forwarde le body brut vers
/// `llama-server`. Plus de backend typé : le client contrôle le sampling, le
/// streaming, les tools, la vision via le body de requête (préservés tel quel).
#[derive(Clone)]
pub struct AppState {
    /// Proxy transparent vers l'enfant llama-server.
    pub proxy: ForwardProxy,
    /// État de warm-up.
    pub health: Arc<HealthState>,
    /// Métriques Prometheus.
    pub metrics: Arc<EngineMetrics>,
    /// Sink d'événements (HttpEventSink ou InMemorySink).
    pub sink: Arc<dyn EventSink>,
    /// Nom du modèle chargé (event-log model_used).
    pub model_name: String,
    /// Alias provider gateway (ex. `"engine-curator"`).
    pub provider: String,
    /// Timeout d'inférence en secondes (504 si dépassé).
    pub timeout_secs: u64,
    /// Limite de taille du body sur le port principal (octets).
    pub body_limit_bytes: usize,
}

// ---------------------------------------------------------------------------
// EngineServer
// ---------------------------------------------------------------------------

/// Constructeur du routeur axum.
pub struct EngineServer;

impl EngineServer {
    /// Construit le routeur principal — port configurable (`bind_addr`).
    ///
    /// Routes : `/health`, `/v1/chat/completions`, `/v1/embeddings` (transparent).
    /// `/metrics` est ABSENT — voir `metrics_router()`.
    /// `DefaultBodyLimit` paramétré par `state.body_limit_bytes`.
    pub fn router(state: AppState) -> Router {
        let body_limit = state.body_limit_bytes;
        Router::new()
            .route("/health", get(health_handler))
            .route("/v1/chat/completions", post(chat_handler))
            .route("/v1/embeddings", post(embed_handler))
            .layer(DefaultBodyLimit::max(body_limit))
            .with_state(state)
    }

    /// Construit le routeur metrics — bindé loopback-only (C2).
    ///
    /// Route : `/metrics` (Prometheus text format).
    /// Ce router est bindé par le binaire sur `127.0.0.1:metrics_port`,
    /// indépendamment de `bind_addr` du port principal.
    pub fn metrics_router(metrics: Arc<EngineMetrics>) -> Router {
        Router::new()
            .route("/metrics", get(metrics_handler))
            .with_state(metrics)
    }

    /// Construit un router de test pointant le `ForwardProxy` vers un child-stub HTTP.
    ///
    /// Usage : tests d'intégration sans `llama-server` réel.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test_app_with_child(
        child_base_url: String,
        timeout_secs: u64,
        body_limit_bytes: usize,
    ) -> Router {
        use gradatum_core::event_sink::InMemorySink;
        // Pas de timeout sur le client reqwest de test : le timeout applicatif est géré
        // exclusivement par `tokio::time::timeout` dans `proxy_request` (→ 504).
        // Un timeout reqwest concurrent masquerait le 504 en 500 (Inference).
        let client = reqwest::Client::builder()
            .build()
            .expect("client reqwest de test");
        let health = Arc::new(HealthState::new("test-model"));
        health.set_ready();
        let state = AppState {
            proxy: ForwardProxy::new(client, child_base_url),
            health,
            metrics: Arc::new(EngineMetrics::new()),
            sink: Arc::new(InMemorySink::default()),
            model_name: "test-model".into(),
            provider: "engine-test".into(),
            timeout_secs,
            body_limit_bytes,
        };
        Self::router(state)
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health_handler(State(s): State<AppState>) -> Json<crate::health::HealthSnapshot> {
    Json(s.health.snapshot())
}

/// Handler pour le router metrics (state = Arc<EngineMetrics>, port loopback-only).
async fn metrics_handler(State(m): State<Arc<EngineMetrics>>) -> (StatusCode, Body) {
    let text = m.render();
    (StatusCode::OK, Body::from(text))
}

async fn chat_handler(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    proxy_request(&s, "/v1/chat/completions", headers, body).await
}

async fn embed_handler(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    proxy_request(&s, "/v1/embeddings", headers, body).await
}

/// Forward transparent partagé : body brut → enfant, réponse brute streamée → client.
///
/// Observe (status_code, latency_ms, model) pour l'event-log + les métriques.
async fn proxy_request(
    s: &AppState,
    subpath: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    let t0 = Instant::now();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let sent = tokio::time::timeout(
        Duration::from_secs(s.timeout_secs),
        s.proxy.forward(subpath, &content_type, body),
    )
    .await;

    let ms = t0.elapsed().as_millis() as u64;

    match sent {
        Err(_elapsed) => {
            s.metrics.record_request(subpath, 504, ms);
            Err((StatusCode::GATEWAY_TIMEOUT, "timeout".into()))
        }
        Ok(Err(e)) => {
            let status = e.status();
            s.metrics.record_request(subpath, status, ms);
            Err((
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.to_string(),
            ))
        }
        Ok(Ok(resp)) => {
            let status = resp.status().as_u16();
            s.metrics.record_request(subpath, status, ms);
            s.sink
                .emit(EngineEvent::RequestServed {
                    route: subpath.to_string(),
                    model: s.model_name.clone(),
                    provider: s.provider.clone(),
                    latency_ms: ms,
                })
                .await;

            // Réponse brute : statut + content-type upstream + corps streamé (SSE pass-through).
            let resp_ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let axum_status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            Response::builder()
                .status(axum_status)
                .header(header::CONTENT_TYPE, resp_ct)
                .body(Body::from_stream(resp.bytes_stream()))
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}

#[cfg(test)]
mod transparent_handler {
    use super::*;
    use axum::body::{to_bytes, Body, Bytes as AxBytes};
    use axum::http::{Request, StatusCode};
    use axum::response::Response as AxResponse;
    use axum::routing::post;
    use axum::Router;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    /// Stub child llama-server : capture le body, renvoie une réponse fixe.
    async fn start_child_stub(
        delay_secs: u64,
        resp_body: &'static str,
    ) -> (u16, Arc<Mutex<Vec<u8>>>) {
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let c1 = captured.clone();
        let c2 = captured.clone();
        let handler = move |body: AxBytes| {
            let cap = c1.clone();
            async move {
                if delay_secs > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                }
                *cap.lock().await = body.to_vec();
                AxResponse::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(resp_body))
                    .unwrap()
            }
        };
        let embed_handler = move |body: AxBytes| {
            let cap = c2.clone();
            async move {
                *cap.lock().await = body.to_vec();
                AxResponse::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from("{\"data\":[{\"embedding\":[0.1],\"index\":0}]}"))
                    .unwrap()
            }
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(handler))
            .route("/v1/embeddings", post(embed_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, captured)
    }

    #[tokio::test]
    async fn chat_forwards_body_with_sampling_and_slot_preserved() {
        let (port, captured) =
            start_child_stub(0, "{\"choices\":[{\"message\":{\"content\":\"ok\"}}]}").await;
        let app = EngineServer::test_app_with_child(
            format!("http://127.0.0.1:{port}"),
            30,
            32 * 1024 * 1024,
        );
        let raw = br#"{"messages":[{"role":"user","content":"hi"}],"temperature":0.7,"slot_id":2,"tools":[],"seed":7}"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(raw.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = captured.lock().await.clone();
        assert_eq!(
            got.as_slice(),
            raw.as_slice(),
            "body transparent : sampling/slot_id/tools/seed préservés"
        );
    }

    #[tokio::test]
    async fn chat_determinism_temperature_zero_preserved() {
        // R-déterminisme curator : temp:0.0 dans la requête → forwardé tel quel (greedy).
        let (port, captured) =
            start_child_stub(0, "{\"choices\":[{\"message\":{\"content\":\"{}\"}}]}").await;
        let app = EngineServer::test_app_with_child(
            format!("http://127.0.0.1:{port}"),
            30,
            32 * 1024 * 1024,
        );
        let raw = br#"{"messages":[{"role":"user","content":"classify"}],"temperature":0.0}"#;
        let _ = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(raw.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let got = captured.lock().await.clone();
        let v: serde_json::Value = serde_json::from_slice(&got).unwrap();
        assert_eq!(
            v["temperature"], 0.0,
            "temperature:0.0 doit être forwardé (déterminisme curator non régressé)"
        );
    }

    #[tokio::test]
    async fn chat_response_body_passed_through() {
        let (port, _) = start_child_stub(
            0,
            "{\"id\":\"child-1\",\"choices\":[{\"message\":{\"content\":\"hi\"}}]}",
        )
        .await;
        let app = EngineServer::test_app_with_child(
            format!("http://127.0.0.1:{port}"),
            30,
            32 * 1024 * 1024,
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        "{\"messages\":[{\"role\":\"user\",\"content\":\"x\"}]}",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["id"], "child-1",
            "réponse child renvoyée telle quelle (pas de réécriture)"
        );
    }

    #[tokio::test]
    async fn chat_returns_504_on_timeout() {
        let (port, _) = start_child_stub(5, "{}").await; // child dort 5s
        let app = EngineServer::test_app_with_child(
            format!("http://127.0.0.1:{port}"),
            1,
            32 * 1024 * 1024,
        ); // timeout 1s
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        "{\"messages\":[{\"role\":\"user\",\"content\":\"x\"}]}",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::GATEWAY_TIMEOUT,
            "timeout < latence child → 504"
        );
    }

    #[tokio::test]
    async fn body_limit_returns_413_over_limit() {
        let (port, _) = start_child_stub(0, "{}").await;
        // Limite minuscule : 64 octets
        let app = EngineServer::test_app_with_child(format!("http://127.0.0.1:{port}"), 30, 64);
        let big = vec![b'a'; 1024]; // 1 KiB > 64
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(big))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "body > body_limit_bytes → 413"
        );
    }

    #[tokio::test]
    async fn embed_forwards_and_returns() {
        let (port, captured) = start_child_stub(0, "{}").await;
        let app = EngineServer::test_app_with_child(
            format!("http://127.0.0.1:{port}"),
            30,
            32 * 1024 * 1024,
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/embeddings")
                    .header("content-type", "application/json")
                    .body(Body::from("{\"input\":\"hello\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = captured.lock().await.clone();
        let v: serde_json::Value = serde_json::from_slice(&got).unwrap();
        assert_eq!(v["input"], "hello", "embed body forwardé transparent");
    }
}
