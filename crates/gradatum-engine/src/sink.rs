//! `HttpEventSink` — poste les événements `RequestServed` vers `/api/v1/event-log`.
//!
//! ## Comportement
//!
//! - Seuls les événements `RequestServed` sont postés (schéma `QaEventDto`).
//! - Les autres événements (lifecycle) sont loggés via `tracing::info!` uniquement.
//! - Best-effort : un échec POST ne bloque jamais l'inférence.
//! - JWT dans `Zeroizing<String>` (P0-8/P2-4).
//!
//! ## Sécurité
//!
//! - `base_url` doit être loopback — validé dans le binaire (P2-4 anti-SSRF).
//! - JWT : jamais loggé, jamais dans un log d'erreur.
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::event_sink::{EngineEvent, EventSink};
use gradatum_dto::QaEventDto;
use zeroize::Zeroizing;

/// Sink HTTP — poste les `RequestServed` vers `/api/v1/event-log` de gradatum-server.
///
/// `base_url` : ex. `"http://127.0.0.1:19090"` (loopback).
/// `jwt` : token JWT 24h obtenu via exchange api-key (P0-8).
pub struct HttpEventSink {
    /// URL de base du serveur gradatum (loopback).
    base_url: String,
    /// JWT dans Zeroizing pour effacement mémoire à la libération.
    jwt: Zeroizing<String>,
    /// Client HTTP réutilisable (pool de connexions interne).
    client: reqwest::Client,
}

impl HttpEventSink {
    /// Construit un `HttpEventSink`.
    ///
    /// # Arguments
    /// - `base_url` : URL de base loopback (ex. `"http://127.0.0.1:19090"`).
    /// - `jwt` : token JWT 24h (P0-8 — pas de JWT statique hardcodé).
    pub fn new(base_url: String, jwt: Zeroizing<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("construction client HTTP — ne devrait pas échouer");
        Self {
            base_url,
            jwt,
            client,
        }
    }
}

#[async_trait]
impl EventSink for HttpEventSink {
    /// Émet un événement.
    ///
    /// - `RequestServed` → POST `/api/v1/event-log` avec `QaEventDto` complet (P0-1).
    /// - Autres variants → `tracing::info!` uniquement (lifecycle → logs).
    async fn emit(&self, event: EngineEvent) {
        match event {
            EngineEvent::RequestServed {
                route,
                model,
                provider,
                latency_ms,
            } => {
                // P0-1 : QaEventDto complet avec tous les champs requis
                let dto = QaEventDto {
                    route: route.clone(),
                    model_alias: model.clone(), // alias = nom du modèle (pas provider)
                    provider: provider.clone(),
                    status_code: 200_u16,
                    latency_ms,
                    timestamp: Utc::now().to_rfc3339(),
                    feature_id: Some("engine".into()),
                    model_used: Some(model),
                    tokens_input: None,
                    tokens_output: None,
                    cost_usd: None,
                    agent_id: None,
                };

                let url = format!("{}/api/v1/event-log", self.base_url);
                // Best-effort N=1 — timeout court (2s) pour ne pas bloquer le serving
                if let Err(e) = self
                    .client
                    .post(&url)
                    .bearer_auth(self.jwt.as_str())
                    .json(&[&dto]) // l'endpoint attend un tableau (Vec<QaEventDto>)
                    .timeout(std::time::Duration::from_secs(2))
                    .send()
                    .await
                {
                    // Jamais FATAL — best-effort — le JWT n'est pas loggé (P2-4)
                    tracing::warn!(
                        route = %route,
                        error_kind = "event_log_post_failed",
                        "HttpEventSink: POST /api/v1/event-log échoué (best-effort)"
                    );
                    let _ = e; // erreur silencieuse — pas de détail pour éviter leak JWT
                }
            }
            other => {
                // Lifecycle events → logs + metrics uniquement, pas event-log
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
        use axum::{routing::post, Json, Router};
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
        );

        sink.emit(EngineEvent::RequestServed {
            route: "/v1/chat/completions".into(),
            model: "qwen3-4b".into(),
            provider: "engine-curator".into(),
            latency_ms: 42,
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
    }

    #[tokio::test]
    async fn lifecycle_events_not_posted() {
        // ModelLoaded ne doit PAS déclencher de POST (lifecycle → logs uniquement)
        let (port, captured) = start_stub_server().await;
        let sink = HttpEventSink::new(
            format!("http://127.0.0.1:{port}"),
            Zeroizing::new("test-jwt".into()),
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
        })
        .await;
        assert_eq!(sink.snapshot().len(), 2);
    }
}
