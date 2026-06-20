//! `HttpEventSink` — posts `RequestServed` events to `/api/v1/event-log`.
//!
//! ## Behaviour
//!
//! - Only `RequestServed` events are posted (schema `QaEventDto`).
//! - Other events (lifecycle) are logged via `tracing::info!` only.
//! - Best-effort: a POST failure never blocks inference.
//! - JWT stored in `Zeroizing<String>` (zeroed on drop).
//!
//! ## Security
//!
//! - `base_url` must be loopback — validated in the binary (anti-SSRF).
//! - JWT is never logged and never included in error messages.
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::event_sink::{EngineEvent, EventSink};
use gradatum_dto::QaEventDto;
use zeroize::Zeroizing;

/// Derives the semantic `feature_id` from the HTTP route.
///
/// Allows the event-log to distinguish the type of request served. Mapping:
/// - route ending with `/embeddings` → `"embed"`
/// - everything else (chat/completions, completions) → `"chat"`
///
/// The comparison is prefix-insensitive: `subpath` may be `/v1/embeddings`
/// or `v1/embeddings` depending on upstream routing. Matches on the suffix
/// `/embeddings` as well as exact equality with `"embeddings"` (no leading slash).
fn derive_feature_id(route: &str) -> &'static str {
    if route.ends_with("/embeddings") || route == "embeddings" {
        "embed"
    } else {
        "chat"
    }
}

/// HTTP sink — posts `RequestServed` events to `/api/v1/event-log` on gradatum-server.
///
/// `base_url`: e.g. `"http://127.0.0.1:19090"` (loopback).
/// `jwt`: 24 h JWT obtained via api-key exchange.
/// `agent_id`: semantic identifier of the emitting engine, propagated as-is in each event.
pub struct HttpEventSink {
    /// Base URL of the gradatum server (loopback).
    base_url: String,
    /// JWT in `Zeroizing` for memory erasure on drop.
    jwt: Zeroizing<String>,
    /// Reusable HTTP client (internal connection pool).
    client: reqwest::Client,
    /// Agent identifier propagated in `QaEventDto.agent_id`.
    ///
    /// `None` = legacy behaviour (`agent_id` absent from the event).
    agent_id: Option<String>,
}

impl HttpEventSink {
    /// Constructs an `HttpEventSink`.
    ///
    /// # Arguments
    /// - `base_url`: loopback base URL (e.g. `"http://127.0.0.1:19090"`).
    /// - `jwt`: 24 h JWT token (no static hardcoded JWT).
    /// - `agent_id`: semantic identifier of the engine; `None` = legacy behaviour.
    pub fn new(base_url: String, jwt: Zeroizing<String>, agent_id: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("construction client HTTP — ne devrait pas échouer");
        Self {
            base_url,
            jwt,
            client,
            agent_id,
        }
    }
}

#[async_trait]
impl EventSink for HttpEventSink {
    /// Emits an event.
    ///
    /// - `RequestServed` → POST `/api/v1/event-log` with a complete `QaEventDto`.
    /// - Other variants → `tracing::info!` only (lifecycle → logs).
    async fn emit(&self, event: EngineEvent) {
        match event {
            EngineEvent::RequestServed {
                route,
                model,
                provider,
                latency_ms,
                status_code,
            } => {
                // Full QaEventDto with all required fields.
                // feature_id derived semantically from the request type
                // (embed vs chat) — no hardcoded "engine" value.
                // agent_id propagated from the engine config.
                let dto = QaEventDto {
                    route: route.clone(),
                    model_alias: model.clone(), // alias = nom du modèle (pas provider)
                    provider: provider.clone(),
                    status_code,
                    latency_ms,
                    timestamp: Utc::now().to_rfc3339(),
                    feature_id: Some(derive_feature_id(&route).to_string()),
                    model_used: Some(model),
                    tokens_input: None,
                    tokens_output: None,
                    cost_usd: None,
                    agent_id: self.agent_id.clone(),
                };

                let url = format!("{}/api/v1/event-log", self.base_url);
                // Best-effort, single attempt — short timeout (2 s) to avoid blocking serving
                if let Err(e) = self
                    .client
                    .post(&url)
                    .bearer_auth(self.jwt.as_str())
                    .json(&[&dto]) // l'endpoint attend un tableau (Vec<QaEventDto>)
                    .timeout(std::time::Duration::from_secs(2))
                    .send()
                    .await
                {
                    // Never fatal — best-effort — JWT is not logged.
                    tracing::warn!(
                        route = %route,
                        error_kind = "event_log_post_failed",
                        "HttpEventSink: POST /api/v1/event-log échoué (best-effort)"
                    );
                    let _ = e; // erreur silencieuse — pas de détail pour éviter leak JWT
                }
            }
            other => {
                // Lifecycle events → logs and metrics only, not the event-log
                tracing::info!(event = ?other, "engine lifecycle event");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_core::event_sink::InMemorySink;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Serveur stub axum sur port éphémère pour capturer les POSTs.
    async fn start_stub_server() -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
        use axum::{Json, Router, routing::post};
        use tokio::net::TcpListener;

        let captured = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let cap2 = captured.clone();

        let app = Router::new().route(
            "/api/v1/event-log",
            post(move |Json(body): Json<serde_json::Value>| {
                let cap = cap2.clone();
                async move {
                    cap.lock().await.push(body);
                    axum::http::StatusCode::OK
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (port, captured)
    }

    #[tokio::test]
    async fn request_served_posts_to_event_log() {
        let (port, captured) = start_stub_server().await;
        let sink = HttpEventSink::new(
            format!("http://127.0.0.1:{port}"),
            Zeroizing::new("test-jwt".into()),
            Some("engine-curator".into()),
        );

        sink.emit(EngineEvent::RequestServed {
            route: "/v1/chat/completions".into(),
            model: "qwen3-4b".into(),
            provider: "engine-curator".into(),
            latency_ms: 42,
            status_code: 200,
        })
        .await;

        // Laisser le temps au POST d'arriver (réseau loopback — 50ms suffisent)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let bodies = captured.lock().await;
        assert_eq!(
            bodies.len(),
            1,
            "un POST attendu pour RequestServed ; reçu {}",
            bodies.len()
        );
        let body = &bodies[0];
        // L'endpoint reçoit un tableau JSON
        let arr = body.as_array().unwrap();
        assert_eq!(arr[0]["route"], "/v1/chat/completions");
        assert_eq!(arr[0]["latency_ms"], 42);
        assert!(
            arr[0]["timestamp"].as_str().is_some(),
            "timestamp RFC3339 présent"
        );
        assert_eq!(arr[0]["status_code"], 200);
        // F-19 M2 : feature_id dérivé (chat pour /v1/chat/completions).
        assert_eq!(
            arr[0]["feature_id"], "chat",
            "feature_id chat dérivé pour route chat/completions (plus de 'engine' hardcodé)"
        );
        // F-19 M1 : agent_id propagé depuis la config.
        assert_eq!(
            arr[0]["agent_id"], "engine-curator",
            "agent_id propagé depuis la config de l'engine"
        );
    }

    /// F-19 M2 : une requête /v1/embeddings produit feature_id="embed".
    #[tokio::test]
    async fn embeddings_route_yields_feature_id_embed() {
        let (port, captured) = start_stub_server().await;
        let sink = HttpEventSink::new(
            format!("http://127.0.0.1:{port}"),
            Zeroizing::new("test-jwt".into()),
            Some("engine-embed".into()),
        );

        sink.emit(EngineEvent::RequestServed {
            route: "/v1/embeddings".into(),
            model: "bge-m3".into(),
            provider: "engine-embed".into(),
            latency_ms: 10,
            status_code: 200,
        })
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let bodies = captured.lock().await;
        let arr = bodies[0].as_array().unwrap();
        assert_eq!(
            arr[0]["feature_id"], "embed",
            "feature_id embed dérivé pour route /v1/embeddings"
        );
        assert_eq!(arr[0]["agent_id"], "engine-embed");
    }

    /// F-19 M1 : agent_id=None (legacy) → champ absent du JSON (skip_serializing_if).
    #[tokio::test]
    async fn agent_id_none_omits_field() {
        let (port, captured) = start_stub_server().await;
        let sink = HttpEventSink::new(
            format!("http://127.0.0.1:{port}"),
            Zeroizing::new("test-jwt".into()),
            None,
        );

        sink.emit(EngineEvent::RequestServed {
            route: "/v1/chat/completions".into(),
            model: "qwen3-4b".into(),
            provider: "engine-curator".into(),
            latency_ms: 42,
            status_code: 200,
        })
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let bodies = captured.lock().await;
        let arr = bodies[0].as_array().unwrap();
        assert!(
            arr[0].get("agent_id").is_none() || arr[0]["agent_id"].is_null(),
            "agent_id None ne doit pas être sérialisé (skip_serializing_if) : {:?}",
            arr[0]
        );
    }

    #[test]
    fn derive_feature_id_maps_routes() {
        assert_eq!(derive_feature_id("/v1/embeddings"), "embed");
        assert_eq!(derive_feature_id("embeddings"), "embed");
        assert_eq!(derive_feature_id("/v1/chat/completions"), "chat");
        assert_eq!(derive_feature_id("/v1/completions"), "chat");
        assert_eq!(derive_feature_id("/health"), "chat", "fallback = chat");
    }

    #[tokio::test]
    async fn lifecycle_events_not_posted() {
        // ModelLoaded ne doit PAS déclencher de POST (lifecycle → logs uniquement)
        let (port, captured) = start_stub_server().await;
        let sink = HttpEventSink::new(
            format!("http://127.0.0.1:{port}"),
            Zeroizing::new("test-jwt".into()),
            None,
        );

        sink.emit(EngineEvent::ModelLoaded {
            model: "test".into(),
        })
        .await;
        sink.emit(EngineEvent::EngineStarted {
            model: "test".into(),
            port: 11435,
        })
        .await;
        sink.emit(EngineEvent::EngineStopping {
            model: "test".into(),
        })
        .await;

        // Délai suffisant pour que d'éventuels POSTs arriveraient
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let bodies = captured.lock().await;
        assert_eq!(
            bodies.len(),
            0,
            "0 POST attendu pour les lifecycle events ; reçu {}",
            bodies.len()
        );
    }

    #[tokio::test]
    async fn in_memory_sink_captures_all_events() {
        // Vérification complémentaire : InMemorySink capture tout (y compris lifecycle)
        let sink = InMemorySink::default();
        sink.emit(EngineEvent::ModelLoaded { model: "x".into() })
            .await;
        sink.emit(EngineEvent::RequestServed {
            route: "/v1/embeddings".into(),
            model: "bge-m3".into(),
            provider: "engine-embed".into(),
            latency_ms: 10,
            status_code: 200,
        })
        .await;
        assert_eq!(sink.snapshot().len(), 2);
    }
}
