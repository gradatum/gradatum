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
//!
//! ## Lazy JWT refresh
//!
//! The stored JWT (TTL 24 h) is refreshed lazily: on an HTTP `401` the sink
//! re-exchanges the api-key for a fresh JWT and retries the POST **once**. This
//! also covers token revocation and an auth-server restart. The refresh is
//! reactive only — there is no background task. As the event-log is best-effort,
//! losing the event that triggered the refresh is acceptable.
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::event_sink::{EngineEvent, EventSink};
use gradatum_dto::QaEventDto;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use zeroize::Zeroizing;

use crate::metrics::EngineMetrics;

/// Error returned by [`exchange_api_key_for_jwt_typed`].
///
/// Typed (rather than `anyhow`) so the caller can act on the variant: an
/// [`Unauthorized`](Self::Unauthorized) at startup is an identity problem that requires
/// human action and will never recover on its own, whereas a [`Transport`](Self::Transport)
/// failure is transient and may recover on restart. The binary uses this distinction to
/// classify the engine's telemetry state (see `TelemetryStatus`).
///
/// # Security
///
/// No variant ever carries the api-key or the JWT — only the target URL and the HTTP
/// status. The `#[source]` `reqwest::Error` is a transport error and never contains the
/// bearer credential.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExchangeError {
    /// The request never reached the server, or the response body could not be read
    /// (connection refused, DNS failure, timeout, TLS error…). Transient.
    #[error("api-key→JWT exchange could not reach {url}")]
    Transport {
        /// The `/auth/exchange` URL that was contacted.
        url: String,
        /// Underlying transport error (never contains the credential).
        #[source]
        source: reqwest::Error,
    },
    /// The server refused the api-key with HTTP 401 — an identity/credential problem
    /// that requires human action (this is the fold reason that lasted ten days).
    #[error("api-key→JWT exchange refused with HTTP 401 Unauthorized ({url})")]
    Unauthorized {
        /// The `/auth/exchange` URL that returned 401.
        url: String,
    },
    /// The server answered with a non-success status other than 401.
    #[error("api-key→JWT exchange → HTTP {status} ({url})")]
    HttpStatus {
        /// The `/auth/exchange` URL that returned the error status.
        url: String,
        /// The non-success HTTP status code (never 401 — see [`Unauthorized`](Self::Unauthorized)).
        status: u16,
    },
    /// The 2xx response body had no usable `token` field.
    #[error("api-key→JWT exchange response from {url} missing 'token' field")]
    MissingToken {
        /// The `/auth/exchange` URL whose response was malformed.
        url: String,
    },
}

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

/// Exchanges an api-key for a 24-hour JWT via `POST /auth/exchange`, returning a typed
/// [`ExchangeError`] on failure.
///
/// The route is mounted outside `/api/v1` (`unauthed.merge(auth_exchange)` in
/// `gradatum-server` — no `/api/v1` prefix). Called at startup by the binary for
/// the initial exchange, and lazily by [`HttpEventSink`] on a `401`.
///
/// This is the fine-grained form: the caller can match on the variant to classify the
/// failure (a `401` identity refusal vs a transport outage) — the distinction that
/// drives the engine's `TelemetryStatus` and the `event_log` field of `/health`.
/// The historic [`anyhow`](anyhow::Result)-returning [`exchange_api_key_for_jwt`] is a
/// thin wrapper over this function, kept for backward compatibility.
///
/// # Errors
///
/// Returns a typed [`ExchangeError`]:
/// - [`Transport`](ExchangeError::Transport) if the request never reaches the server or
///   the body cannot be read (transient);
/// - [`Unauthorized`](ExchangeError::Unauthorized) on HTTP 401 (identity problem);
/// - [`HttpStatus`](ExchangeError::HttpStatus) on any other non-2xx status;
/// - [`MissingToken`](ExchangeError::MissingToken) if the 2xx body has no `token` field.
///
/// No error message ever contains the api-key or the JWT.
#[must_use = "the exchange result determines whether the event-log is active or folds"]
pub async fn exchange_api_key_for_jwt_typed(
    api_key: &Zeroizing<String>,
    base_url: &str,
) -> Result<Zeroizing<String>, ExchangeError> {
    let url = format!("{base_url}/auth/exchange");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|source| ExchangeError::Transport {
            url: url.clone(),
            source,
        })?;
    let resp = client
        .post(&url)
        .bearer_auth(api_key.as_str())
        .send()
        .await
        .map_err(|source| ExchangeError::Transport {
            url: url.clone(),
            source,
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(if status == reqwest::StatusCode::UNAUTHORIZED {
            ExchangeError::Unauthorized { url }
        } else {
            ExchangeError::HttpStatus {
                url,
                status: status.as_u16(),
            }
        });
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|source| ExchangeError::Transport {
            url: url.clone(),
            source,
        })?;
    let token = body["token"]
        .as_str()
        .ok_or(ExchangeError::MissingToken { url })?;
    Ok(Zeroizing::new(token.to_string()))
}

/// Exchanges an api-key for a 24-hour JWT — **compatibility wrapper** returning
/// [`anyhow::Result`].
///
/// This is the historic signature, preserved so external consumers pinned on `2.0` keep
/// compiling after a `cargo update`. It simply delegates to
/// [`exchange_api_key_for_jwt_typed`] and erases the typed [`ExchangeError`] into
/// [`anyhow::Error`] via the `?` conversion (`ExchangeError` implements
/// [`std::error::Error`] through `thiserror`). The underlying [`ExchangeError`] is kept
/// as the error source and can be recovered with [`anyhow::Error::downcast_ref`].
///
/// New code inside this crate should call [`exchange_api_key_for_jwt_typed`] instead and
/// act on the variant — that fine classification is what matters here: an identity
/// refusal must not be mislabelled as a transient outage.
///
/// # Errors
///
/// Same failure conditions as [`exchange_api_key_for_jwt_typed`], erased into
/// [`anyhow::Error`]. No error message ever contains the api-key or the JWT.
#[must_use = "the exchange result determines whether the event-log is active or folds"]
pub async fn exchange_api_key_for_jwt(
    api_key: &Zeroizing<String>,
    base_url: &str,
) -> anyhow::Result<Zeroizing<String>> {
    Ok(exchange_api_key_for_jwt_typed(api_key, base_url).await?)
}

/// HTTP sink — posts `RequestServed` events to `/api/v1/event-log` on gradatum-server.
///
/// `base_url`: e.g. `"http://127.0.0.1:19090"` (loopback).
/// `jwt`: 24 h JWT obtained via api-key exchange, refreshed lazily on `401`.
/// `agent_id`: semantic identifier of the emitting engine, propagated as-is in each event.
pub struct HttpEventSink {
    /// Base URL of the gradatum server (loopback).
    base_url: String,
    /// JWT in `Zeroizing` for memory erasure on drop.
    ///
    /// Behind a `RwLock` for interior mutability: `emit(&self, …)` reads it on
    /// every POST and rewrites it on a lazy refresh. Reads dominate.
    jwt: RwLock<Zeroizing<String>>,
    /// api-key used to re-exchange a fresh JWT on `401` (never logged).
    api_key: Zeroizing<String>,
    /// Reusable HTTP client (internal connection pool).
    client: reqwest::Client,
    /// Agent identifier propagated in `QaEventDto.agent_id`.
    ///
    /// `None` = legacy behaviour (`agent_id` absent from the event).
    agent_id: Option<String>,
    /// Shared engine metrics — non-2xx / undelivered POSTs are counted here.
    metrics: Arc<EngineMetrics>,
}

impl HttpEventSink {
    /// Constructs an `HttpEventSink`.
    ///
    /// # Arguments
    /// - `base_url`: loopback base URL (e.g. `"http://127.0.0.1:19090"`).
    /// - `jwt`: initial 24 h JWT token (no static hardcoded JWT).
    /// - `api_key`: api-key kept to re-exchange a fresh JWT on `401`.
    /// - `agent_id`: semantic identifier of the engine; `None` = legacy behaviour.
    /// - `metrics`: shared metrics used to count event-log delivery failures.
    pub fn new(
        base_url: String,
        jwt: Zeroizing<String>,
        api_key: Zeroizing<String>,
        agent_id: Option<String>,
        metrics: Arc<EngineMetrics>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("HTTP client construction — should not fail");
        Self {
            base_url,
            jwt: RwLock::new(jwt),
            api_key,
            client,
            agent_id,
            metrics,
        }
    }

    /// Posts the serialized event batch once and returns the HTTP status.
    ///
    /// Factored out so the nominal attempt and the post-refresh retry share the
    /// exact same request shape (bearer, JSON array body, 2 s timeout). The lock
    /// on the JWT is never held across this `await`: the caller passes a snapshot
    /// `token` by value.
    async fn post_event_log(
        &self,
        url: &str,
        token: &str,
        body: &[&QaEventDto],
    ) -> Result<reqwest::StatusCode, reqwest::Error> {
        let resp = self
            .client
            .post(url)
            .bearer_auth(token)
            .json(body)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await?;
        Ok(resp.status())
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
                // l'endpoint attend un tableau (Vec<QaEventDto>)
                let body = [&dto];

                // Snapshot the current JWT — the read lock is dropped before the
                // POST await (never hold a lock across .await, ADN 2).
                let token = { self.jwt.read().await.clone() };

                // Best-effort: a failure never blocks inference. The HTTP status
                // is now inspected (F-120) — a 401 is no longer a silent success.
                match self.post_event_log(&url, token.as_str(), &body).await {
                    Ok(status) if status.is_success() => {
                        // Delivered — nothing to do.
                    }
                    Ok(status) if status == reqwest::StatusCode::UNAUTHORIZED => {
                        // JWT expired or revoked → count the 401, refresh, retry once.
                        self.metrics.record_event_log_error(status.as_str());
                        match exchange_api_key_for_jwt_typed(&self.api_key, &self.base_url).await {
                            Ok(fresh) => {
                                // Update the stored JWT (write lock dropped before
                                // the retry await — no lock across .await).
                                {
                                    *self.jwt.write().await = fresh.clone();
                                }
                                match self.post_event_log(&url, fresh.as_str(), &body).await {
                                    Ok(retry_status) if retry_status.is_success() => {
                                        // Recovered after refresh.
                                    }
                                    Ok(retry_status) => {
                                        self.metrics.record_event_log_error(retry_status.as_str());
                                        tracing::warn!(
                                            route = %route,
                                            status = retry_status.as_str(),
                                            error_kind = "event_log_retry_non2xx",
                                            "HttpEventSink: retry after JWT refresh still non-2xx (best-effort)"
                                        );
                                    }
                                    Err(_e) => {
                                        self.metrics.record_event_log_error("transport");
                                        tracing::warn!(
                                            route = %route,
                                            error_kind = "event_log_retry_transport_failed",
                                            "HttpEventSink: retry POST after JWT refresh failed at transport level (best-effort)"
                                        );
                                    }
                                }
                            }
                            Err(_e) => {
                                // Refresh failed — the 401 is already counted. No
                                // JWT/api-key detail is logged (exchange errors
                                // never contain secrets, but stay conservative).
                                tracing::warn!(
                                    route = %route,
                                    error_kind = "event_log_jwt_refresh_failed",
                                    "HttpEventSink: api-key→JWT refresh failed on 401 (best-effort)"
                                );
                            }
                        }
                    }
                    Ok(status) => {
                        // Other non-2xx (4xx/5xx) — count, warn, no retry.
                        self.metrics.record_event_log_error(status.as_str());
                        tracing::warn!(
                            route = %route,
                            status = status.as_str(),
                            error_kind = "event_log_non2xx",
                            "HttpEventSink: POST /api/v1/event-log returned non-2xx (best-effort)"
                        );
                    }
                    Err(_e) => {
                        // Transport error (connection refused, timeout) — count, warn.
                        // JWT is not logged; no detail to avoid any leak.
                        self.metrics.record_event_log_error("transport");
                        tracing::warn!(
                            route = %route,
                            error_kind = "event_log_post_failed",
                            "HttpEventSink: POST /api/v1/event-log failed at transport level (best-effort)"
                        );
                    }
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

    /// Axum stub server on an ephemeral port for capturing POST bodies.
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
            Zeroizing::new("test-api-key".into()),
            Some("engine-curator".into()),
            Arc::new(EngineMetrics::new()),
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

    /// A `/v1/embeddings` request produces `feature_id = "embed"`.
    #[tokio::test]
    async fn embeddings_route_yields_feature_id_embed() {
        let (port, captured) = start_stub_server().await;
        let sink = HttpEventSink::new(
            format!("http://127.0.0.1:{port}"),
            Zeroizing::new("test-jwt".into()),
            Zeroizing::new("test-api-key".into()),
            Some("engine-embed".into()),
            Arc::new(EngineMetrics::new()),
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

    /// When `agent_id` is `None`, the field is absent from the serialized JSON (`skip_serializing_if`).
    #[tokio::test]
    async fn agent_id_none_omits_field() {
        let (port, captured) = start_stub_server().await;
        let sink = HttpEventSink::new(
            format!("http://127.0.0.1:{port}"),
            Zeroizing::new("test-jwt".into()),
            Zeroizing::new("test-api-key".into()),
            None,
            Arc::new(EngineMetrics::new()),
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
            Zeroizing::new("test-api-key".into()),
            None,
            Arc::new(EngineMetrics::new()),
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

    // --- F-120 : refresh paresseux du JWT + comptage non-2xx ---

    /// Stub qui simule un JWT périmé : le 1er POST event-log renvoie 401, les
    /// suivants 200 (avec capture du body). Monte aussi `/auth/exchange` qui
    /// renvoie un token rafraîchi et compte ses appels.
    async fn start_stub_server_refresh() -> (
        u16,
        Arc<Mutex<Vec<serde_json::Value>>>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;

        let captured = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let eventlog_calls = Arc::new(AtomicUsize::new(0));
        let exchange_calls = Arc::new(AtomicUsize::new(0));

        let cap = captured.clone();
        let ev_calls = eventlog_calls.clone();
        let ex_calls = exchange_calls.clone();

        let app = Router::new()
            .route(
                "/auth/exchange",
                post(move || {
                    let ex = ex_calls.clone();
                    async move {
                        ex.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "token": "refreshed-jwt-xyz" }))
                    }
                }),
            )
            .route(
                "/api/v1/event-log",
                post(move |Json(body): Json<serde_json::Value>| {
                    let cap = cap.clone();
                    let ev = ev_calls.clone();
                    async move {
                        let n = ev.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            // Premier appel : JWT périmé → 401.
                            StatusCode::UNAUTHORIZED
                        } else {
                            // Retry (JWT rafraîchi) → capture + 200.
                            cap.lock().await.push(body);
                            StatusCode::OK
                        }
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (port, captured, exchange_calls)
    }

    /// Stub dont `/api/v1/event-log` renvoie toujours 500 (aucun retry attendu).
    async fn start_stub_server_500() -> u16 {
        use axum::{Router, http::StatusCode, routing::post};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/api/v1/event-log",
            post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    /// Sur 401, le sink ré-échange l'api-key contre un JWT frais, retry une fois,
    /// livre l'événement, met à jour le JWT stocké et compte le 401 (F-120).
    #[tokio::test]
    async fn refreshes_jwt_on_401_and_retries() {
        let (port, captured, exchange_calls) = start_stub_server_refresh().await;
        let metrics = Arc::new(EngineMetrics::new());
        let sink = HttpEventSink::new(
            format!("http://127.0.0.1:{port}"),
            Zeroizing::new("stale-jwt".into()),
            Zeroizing::new("test-api-key".into()),
            Some("engine-curator".into()),
            metrics.clone(),
        );

        sink.emit(EngineEvent::RequestServed {
            route: "/v1/chat/completions".into(),
            model: "qwen3-4b".into(),
            provider: "engine-curator".into(),
            latency_ms: 42,
            status_code: 200,
        })
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Un seul re-exchange api-key→JWT déclenché par le 401.
        assert_eq!(
            exchange_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "un re-exchange api-key→JWT attendu sur 401"
        );

        // Le retry a livré le body (capturé sur le 2e POST, en 200).
        {
            let bodies = captured.lock().await;
            assert_eq!(bodies.len(), 1, "le retry doit livrer l'événement");
            let arr = bodies[0].as_array().unwrap();
            assert_eq!(arr[0]["route"], "/v1/chat/completions");
        }

        // Le JWT stocké a été remplacé par le token rafraîchi.
        {
            let stored = sink.jwt.read().await;
            assert_eq!(
                stored.as_str(),
                "refreshed-jwt-xyz",
                "JWT stocké mis à jour après refresh"
            );
        }

        // Le 401 initial est compté comme non-2xx (observabilité F-120).
        let out = metrics.render();
        assert!(
            out.contains("engine_event_log_errors_total{status_code=\"401\"} 1"),
            "le 401 doit être compté une fois : {out}"
        );
    }

    /// Un 500 est compté comme non-2xx, sans retry, sans panic ni blocage (F-120).
    #[tokio::test]
    async fn counts_non2xx_500_without_retry_or_panic() {
        let port = start_stub_server_500().await;
        let metrics = Arc::new(EngineMetrics::new());
        let sink = HttpEventSink::new(
            format!("http://127.0.0.1:{port}"),
            Zeroizing::new("test-jwt".into()),
            Zeroizing::new("test-api-key".into()),
            None,
            metrics.clone(),
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

        // Un 500 compté exactement une fois, pas de retry (le retry n'existe que sur 401).
        let out = metrics.render();
        assert!(
            out.contains("engine_event_log_errors_total{status_code=\"500\"} 1"),
            "un 500 doit être compté une fois (pas de retry) : {out}"
        );
    }

    // --- F-205 : l'échange d'api-key discrimine ses motifs d'échec ---

    /// Monte `/auth/exchange` renvoyant le `status` fourni. Sur 200, répond un token.
    async fn start_stub_auth_exchange(status: u16) -> u16 {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use tokio::net::TcpListener;

        let code = StatusCode::from_u16(status).expect("code HTTP de test valide");
        let app = Router::new().route(
            "/auth/exchange",
            post(move || async move {
                if code.is_success() {
                    Json(serde_json::json!({ "token": "fresh-jwt-abc" })).into_response()
                } else {
                    code.into_response()
                }
            }),
        );
        // `into_response` requiert le trait en scope.
        use axum::response::IntoResponse as _;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    /// Un 401 sur `/auth/exchange` produit `ExchangeError::Unauthorized` — le motif
    /// « problème d'identité » qui a duré dix jours, distinct d'une panne transitoire.
    #[tokio::test]
    async fn exchange_maps_401_to_unauthorized() {
        let port = start_stub_auth_exchange(401).await;
        let api_key = Zeroizing::new("ak_testkey".into());
        let err = exchange_api_key_for_jwt_typed(&api_key, &format!("http://127.0.0.1:{port}"))
            .await
            .expect_err("un 401 doit produire une erreur");
        assert!(
            matches!(err, ExchangeError::Unauthorized { .. }),
            "401 → Unauthorized, obtenu : {err:?}"
        );
        // L'api-key ne doit jamais fuiter dans le message d'erreur.
        assert!(
            !err.to_string().contains("ak_testkey"),
            "le message d'erreur ne doit pas contenir l'api-key"
        );
    }

    /// Un non-2xx autre que 401 (ici 500) produit `ExchangeError::HttpStatus`, jamais
    /// mal étiqueté en injoignable ni en 401.
    #[tokio::test]
    async fn exchange_maps_non401_to_http_status() {
        let port = start_stub_auth_exchange(500).await;
        let api_key = Zeroizing::new("ak_testkey".into());
        let err = exchange_api_key_for_jwt_typed(&api_key, &format!("http://127.0.0.1:{port}"))
            .await
            .expect_err("un 500 doit produire une erreur");
        assert!(
            matches!(err, ExchangeError::HttpStatus { status: 500, .. }),
            "500 → HttpStatus{{500}}, obtenu : {err:?}"
        );
    }

    /// Un serveur injoignable (port fermé) produit `ExchangeError::Transport` — motif
    /// transitoire, distinct d'un refus d'identité.
    #[tokio::test]
    async fn exchange_maps_unreachable_to_transport() {
        // Réserve un port puis le libère : la connexion sera refusée.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let api_key = Zeroizing::new("ak_testkey".into());
        let err = exchange_api_key_for_jwt_typed(&api_key, &format!("http://127.0.0.1:{port}"))
            .await
            .expect_err("un serveur injoignable doit produire une erreur");
        assert!(
            matches!(err, ExchangeError::Transport { .. }),
            "injoignable → Transport, obtenu : {err:?}"
        );
    }

    /// Un 200 sans champ `token` produit `ExchangeError::MissingToken`.
    #[tokio::test]
    async fn exchange_maps_missing_token() {
        use axum::{Json, Router, routing::post};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/auth/exchange",
            post(|| async { Json(serde_json::json!({ "not_token": "x" })) }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let api_key = Zeroizing::new("ak_testkey".into());
        let err = exchange_api_key_for_jwt_typed(&api_key, &format!("http://127.0.0.1:{port}"))
            .await
            .expect_err("un corps sans 'token' doit produire une erreur");
        assert!(
            matches!(err, ExchangeError::MissingToken { .. }),
            "corps sans token → MissingToken, obtenu : {err:?}"
        );
    }
}
