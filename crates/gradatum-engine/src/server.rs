//! Axum server for `gradatum-engine` — OpenAI-compatible routes + `/health` + `/metrics`.
//!
//! ## Routes — main port (configurable `bind_addr`)
//!
//! | Method | Path | Auth | Description |
//! |--------|------|------|-------------|
//! | GET | `/health` | no | Warm-up state and backend identifier |
//! | POST | `/v1/chat/completions` | no | Chat generation (transparent proxy) |
//! | POST | `/v1/embeddings` | no | Embeddings (transparent proxy) |
//!
//! ## Routes — metrics port (loopback-only, `127.0.0.1:metrics_port`)
//!
//! | Method | Path | Auth | Description |
//! |--------|------|------|-------------|
//! | GET | `/metrics` | no | Prometheus text-format metrics |
//!
//! `/metrics` is intentionally absent from the main port. In LAN configurations
//! (`bind_addr` = LAN IP), metrics must never be reachable outside loopback.
//! The metrics listener is managed by the binary on `127.0.0.1:metrics_port`.
//!
//! ## Security
//!
//! - `bind_addr` is fail-closed: validated in `EngineConfig::validate()`.
//! - `DefaultBodyLimit` is configurable via `body_limit_bytes` (default 32 MiB for vision).
//! - Timeout via `tokio::time::timeout` → HTTP 504.
//!
//! ## Transparent reverse proxy
//!
//! The typed `GenBackend` / `ProxyBackend` is removed — `ForwardProxy` (reqwest)
//! forwards the raw request body to `llama-server`. This automatically preserves
//! `slot_id`, sampling parameters, tools, vision, `seed`, `response_format`, and SSE
//! streaming. The control/ops plane (supervisor, health, metrics, bind, `env_clear`) is
//! unchanged.
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

/// Shared state injected into axum handlers.
///
/// `proxy` is a `ForwardProxy` (reqwest) that forwards the raw request body to
/// `llama-server`. There is no typed backend: the client controls sampling,
/// streaming, tools, and vision through the request body (preserved as-is).
#[derive(Clone)]
pub struct AppState {
    /// Transparent proxy to the child `llama-server`.
    pub proxy: ForwardProxy,
    /// Warm-up state.
    pub health: Arc<HealthState>,
    /// Prometheus metrics.
    pub metrics: Arc<EngineMetrics>,
    /// Event sink (`HttpEventSink` or `InMemorySink`).
    pub sink: Arc<dyn EventSink>,
    /// Name of the loaded model (event-log `model_used`).
    pub model_name: String,
    /// Gateway provider alias (e.g. `"engine-curator"`).
    pub provider: String,
    /// Inference timeout in seconds (HTTP 504 on expiry).
    pub timeout_secs: u64,
    /// Request body size limit on the main port (bytes).
    pub body_limit_bytes: usize,
}

// ---------------------------------------------------------------------------
// EngineServer
// ---------------------------------------------------------------------------

/// Builder for the axum router.
pub struct EngineServer;

impl EngineServer {
    /// Builds the main router — configurable port (`bind_addr`).
    ///
    /// Routes: `/health`, `/v1/chat/completions`, `/v1/embeddings` (transparent).
    /// `/metrics` is ABSENT — see `metrics_router()`.
    /// `DefaultBodyLimit` is set from `state.body_limit_bytes`.
    pub fn router(state: AppState) -> Router {
        let body_limit = state.body_limit_bytes;
        Router::new()
            .route("/health", get(health_handler))
            .route("/v1/chat/completions", post(chat_handler))
            .route("/v1/embeddings", post(embed_handler))
            .layer(DefaultBodyLimit::max(body_limit))
            .with_state(state)
    }

    /// Builds the metrics router — bound loopback-only.
    ///
    /// Route: `/metrics` (Prometheus text format).
    /// This router is bound by the binary on `127.0.0.1:metrics_port`,
    /// independently of the main port's `bind_addr`.
    pub fn metrics_router(metrics: Arc<EngineMetrics>) -> Router {
        Router::new()
            .route("/metrics", get(metrics_handler))
            .with_state(metrics)
    }

    /// Builds a test router pointing the `ForwardProxy` at an HTTP child stub.
    ///
    /// Used in integration tests without a real `llama-server`.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test_app_with_child(
        child_base_url: String,
        timeout_secs: u64,
        body_limit_bytes: usize,
    ) -> Router {
        use gradatum_core::event_sink::InMemorySink;
        // No timeout on the test reqwest client: the application timeout is managed
        // exclusively by `tokio::time::timeout` inside `proxy_request` (→ 504).
        // A concurrent reqwest timeout would mask the 504 as a 500 (Inference).
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

/// Handler for the metrics router (state = `Arc<EngineMetrics>`, loopback-only port).
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

/// Shared transparent forward: raw body → child, raw response streamed → client.
///
/// Records (status_code, latency_ms, model) for the event-log and metrics.
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

            // Emit RequestServed with the real upstream status code.
            // On upstream 5xx, do NOT emit — a 5xx indicates a model error, not a
            // successfully served request (avoids polluting the event-log).
            // On 2xx and 4xx: emit with the real code (not hardcoded 200).
            if status < 500 {
                s.sink
                    .emit(EngineEvent::RequestServed {
                        route: subpath.to_string(),
                        model: s.model_name.clone(),
                        provider: s.provider.clone(),
                        latency_ms: ms,
                        status_code: status,
                    })
                    .await;
            } else {
                // 5xx upstream → log only, no event-log.
                tracing::warn!(
                    route = %subpath,
                    status = status,
                    latency_ms = ms,
                    "upstream 5xx — RequestServed non émis (event-log)"
                );
            }

            // Raw response: upstream status + content-type + streamed body (SSE pass-through).
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

    // ── P2 item 3 : status_code réel + pas de RequestServed sur 5xx ──────────

    /// Stub child qui renvoie un code HTTP configurable.
    async fn start_child_stub_with_status(status_code: u16) -> u16 {
        use axum::response::IntoResponse;
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move || async move {
                (
                    axum::http::StatusCode::from_u16(status_code)
                        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
                    axum::Json(serde_json::json!({"error": "upstream error"})),
                )
                    .into_response()
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    /// Helper : construit un AppState avec InMemorySink récupérable.
    async fn make_app_with_sink(
        child_port: u16,
    ) -> (Router, Arc<gradatum_core::event_sink::InMemorySink>) {
        let client = reqwest::Client::builder().build().unwrap();
        let health = Arc::new(crate::health::HealthState::new("test-model"));
        health.set_ready();
        let sink = Arc::new(gradatum_core::event_sink::InMemorySink::default());
        let state = AppState {
            proxy: crate::runtime::ForwardProxy::new(
                client,
                format!("http://127.0.0.1:{child_port}"),
            ),
            health,
            metrics: Arc::new(crate::metrics::EngineMetrics::new()),
            sink: sink.clone() as Arc<dyn gradatum_core::event_sink::EventSink>,
            model_name: "test-model".into(),
            provider: "engine-test".into(),
            timeout_secs: 30,
            body_limit_bytes: 32 * 1024 * 1024,
        };
        (EngineServer::router(state), sink)
    }

    /// P2 item 3 : RequestServed émis sur 200 avec le status réel (pas hardcodé).
    #[tokio::test]
    async fn request_served_emitted_with_real_status_200() {
        let port = start_child_stub_with_status(200).await;
        let (app, sink) = make_app_with_sink(port).await;

        let _ = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        let events = sink.snapshot();
        assert_eq!(events.len(), 1, "un RequestServed attendu sur 200");
        assert!(
            matches!(
                events[0],
                gradatum_core::event_sink::EngineEvent::RequestServed {
                    status_code: 200,
                    ..
                }
            ),
            "status_code doit être 200 (réel, pas hardcodé) — {:?}",
            events[0]
        );
    }

    /// P2 item 3 : RequestServed émis sur 4xx avec le status réel.
    #[tokio::test]
    async fn request_served_emitted_with_real_status_4xx() {
        let port = start_child_stub_with_status(422).await;
        let (app, sink) = make_app_with_sink(port).await;

        let _ = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        let events = sink.snapshot();
        assert_eq!(events.len(), 1, "un RequestServed attendu sur 4xx");
        assert!(
            matches!(
                events[0],
                gradatum_core::event_sink::EngineEvent::RequestServed {
                    status_code: 422,
                    ..
                }
            ),
            "status_code doit être 422 (réel 4xx) — {:?}",
            events[0]
        );
    }

    /// P2 item 3 : PAS de RequestServed sur 5xx upstream.
    #[tokio::test]
    async fn request_served_not_emitted_on_5xx() {
        let port = start_child_stub_with_status(503).await;
        let (app, sink) = make_app_with_sink(port).await;

        let _ = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        let events = sink.snapshot();
        assert_eq!(
            events.len(),
            0,
            "aucun RequestServed ne doit être émis sur 5xx upstream — reçu: {events:?}"
        );
    }
}
