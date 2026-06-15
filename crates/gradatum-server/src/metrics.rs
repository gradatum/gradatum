//! Prometheus metrics — loopback side-channel.
//!
//! Bound exclusively to loopback (127.0.0.1:19091 by default). Not configurable
//! on a non-loopback address (no TLS escape, unlike the main bind).
//!
//! Label cardinality is capped (default 100/series). Labels are sanitized
//! via a static allowlist — paths use route templates, never concrete URIs
//! (e.g., `/api/v1/vault_search` not `/api/v1/vault_search?q=secret`).
//!
//! # Declared metrics
//!
//! | Nom | Type | Notes |
//! |---|---|---|
//! | `gradatum_http_requests_total` | Counter | method, path (template), status |
//! | `gradatum_http_request_duration_seconds` | Histogram | method, path (template) |
//! | `gradatum_queue_depth` | Gauge | tenant |
//! | `gradatum_queue_lag_seconds` | Gauge | tenant |
//! | `gradatum_auth_failures_total` | Counter | reason |
//! | `gradatum_revocation_store_size` | Gauge | (sans label) |
//! | `gradatum_curator_decisions_total` | Counter | action — stub T11, impl P2.0b |
//! | `gradatum_llm_backend_calls_total` | Counter | backend, outcome — stub T11, impl P2.0b |

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{body::Body, extract::State, http::StatusCode, response::Response};
use prometheus_client::{
    encoding::text::encode,
    metrics::{
        counter::Counter,
        family::Family,
        gauge::Gauge,
        histogram::{exponential_buckets, Histogram},
    },
    registry::Registry,
};

// ---------------------------------------------------------------------------
// Label sets
// ---------------------------------------------------------------------------

/// Labels for HTTP requests — sanitized paths (templates, never concrete URIs).
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct HttpReqLabels {
    /// HTTP method (GET, POST, …).
    pub method: String,
    /// Route template (e.g., `/api/v1/vault_search`).
    pub path: &'static str,
    /// HTTP response status code (200, 400, 500, …).
    pub status: u16,
}

/// tenant label — controlled by cardinality cap.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct TenantLabel {
    pub tenant: String,
}

/// Auth failure reason label.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct AuthFailLabel {
    /// Failure reason (e.g., `"invalid_token"`, `"expired"`, `"revoked"`).
    pub reason: &'static str,
}

/// Label action curator — stub T11, impl effective P2.0b.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct CuratorActionLabel {
    pub action: &'static str,
}

/// Labels appel LLM backend — stub T11, impl effective P2.0b.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct LlmBackendLabel {
    pub backend: &'static str,
    pub outcome: &'static str,
}

// ---------------------------------------------------------------------------
// AppMetrics
// ---------------------------------------------------------------------------

/// Application metrics exported on the loopback side-channel :19091.
///
/// Cloneable — the `Registry` and families are wrapped in `Arc`.
/// Injected into `AppState` and into the separate metrics router.
///
/// Fields are `pub` to be accessible by HTTP middlewares and handlers.
/// `dead_code` suppressed: fields are intentional stubs — wired in a future release.
#[allow(dead_code)]
#[derive(Clone)]
pub struct AppMetrics {
    /// Prometheus registry (shared via Arc to allow cloning).
    pub registry: Arc<Registry>,

    // -- Métriques HTTP -------------------------------------------------------
    /// Total HTTP requests (method, path template, status).
    pub http_requests: Family<HttpReqLabels, Counter>,
    /// HTTP request duration in seconds (method, path template).
    pub http_duration: Family<HttpReqLabels, Histogram>,

    // -- File d'attente -------------------------------------------------------
    /// Write queue depth per tenant (label controlled by cap).
    pub queue_depth: Family<TenantLabel, Gauge>,
    /// Write queue lag in seconds per tenant (label controlled by cap).
    pub queue_lag: Family<TenantLabel, Gauge>,

    // -- Auth -----------------------------------------------------------------
    /// Auth failure count by reason.
    pub auth_failures: Family<AuthFailLabel, Counter>,
    /// Revocation store size (number of entries).
    pub revocation_size: Gauge,

    // -- Curator / LLM (stubs T11 — impl effective P2.0b) --------------------
    /// curator decisions by action — intentional stub.
    pub curator_decisions: Family<CuratorActionLabel, Counter>,
    /// LLM backend calls by backend+outcome — intentional stub.
    pub llm_calls: Family<LlmBackendLabel, Counter>,

    // -- Event-log (B1 tranche v0.3.0) ----------------------------------------
    /// Current row count in the `event_log` table.
    ///
    /// Updated by the tokio interval retention task (every 6h).
    /// Not to be called from handlers (full scan — lazy only).
    pub event_log_rows: Gauge,

    // -- Cardinality cap (tenant) --------------------------------------------
    /// Number of distinct tenant labels registered so far.
    tenant_count: Arc<AtomicUsize>,
    /// Cardinality cap per tenant series (default: 100).
    cap: usize,
}

impl AppMetrics {
    /// Creates and registers the 8 metrics in a new `Registry`.
    ///
    /// # Histogram buckets
    /// HTTP duration: 10 exponential values starting from 1ms (base 2),
    /// covering ~1ms – ~1s.
    pub fn new() -> Self {
        // Les familles doivent être clonées AVANT register (register prend ownership d'une copie).
        let http_requests: Family<HttpReqLabels, Counter> = Family::default();
        let http_duration: Family<HttpReqLabels, Histogram> =
            Family::new_with_constructor(|| Histogram::new(exponential_buckets(0.001, 2.0, 10)));
        let queue_depth: Family<TenantLabel, Gauge> = Family::default();
        let queue_lag: Family<TenantLabel, Gauge> = Family::default();
        let auth_failures: Family<AuthFailLabel, Counter> = Family::default();
        let revocation_size: Gauge = Gauge::default();
        let curator_decisions: Family<CuratorActionLabel, Counter> = Family::default();
        let llm_calls: Family<LlmBackendLabel, Counter> = Family::default();
        let event_log_rows: Gauge = Gauge::default();

        let mut registry = Registry::default();

        registry.register(
            "gradatum_http_requests",
            "Nombre total de requêtes HTTP reçues",
            http_requests.clone(),
        );
        registry.register(
            "gradatum_http_request_duration_seconds",
            "Durée des requêtes HTTP en secondes",
            http_duration.clone(),
        );
        registry.register(
            "gradatum_queue_depth",
            "Profondeur de la file d'écriture par tenant",
            queue_depth.clone(),
        );
        registry.register(
            "gradatum_queue_lag_seconds",
            "Décalage de la file d'écriture en secondes par tenant",
            queue_lag.clone(),
        );
        registry.register(
            "gradatum_auth_failures",
            "Nombre d'échecs d'authentification par raison",
            auth_failures.clone(),
        );
        registry.register(
            "gradatum_revocation_store_size",
            "Nombre d'entrées dans le store de révocation",
            revocation_size.clone(),
        );
        registry.register(
            "gradatum_curator_decisions",
            "Décisions curator par action (stub T11)",
            curator_decisions.clone(),
        );
        registry.register(
            "gradatum_llm_backend_calls",
            "Appels LLM backend par backend+outcome (stub T11)",
            llm_calls.clone(),
        );
        registry.register(
            "gradatum_event_log_rows",
            "Nombre de lignes courantes dans event_log (mis à jour par la tâche de rétention)",
            event_log_rows.clone(),
        );

        Self {
            registry: Arc::new(registry),
            http_requests,
            http_duration,
            queue_depth,
            queue_lag,
            auth_failures,
            revocation_size,
            curator_decisions,
            llm_calls,
            event_log_rows,
            tenant_count: Arc::new(AtomicUsize::new(0)),
            cap: 100,
        }
    }

    /// Registers a tenant label, applying the cardinality cap.
    // Utilisé par les middlewares HTTP (T12+) et directement par les tests.
    #[allow(dead_code)]
    ///
    /// # Behavior
    /// - If cardinality has not yet reached the cap, increments the counter
    ///   and returns `Some(label)` — the caller can use this label to observe metrics.
    /// - If the cap is reached, logs a warning and returns `None` — the label is dropped.
    ///
    /// # Important note
    /// This counter is an _admission_ counter: it tallies unique labels seen
    /// for the first time. It has no knowledge of labels already created in the Family.
    /// For correct usage: call `observe_tenant` once per distinct tenant,
    /// then reuse the label directly for subsequent metric updates.
    pub fn observe_tenant(&self, label: TenantLabel) -> Option<TenantLabel> {
        let current = self.tenant_count.load(Ordering::Relaxed);
        if current >= self.cap {
            tracing::warn!(
                tenant = %label.tenant,
                cap = self.cap,
                "cardinality cap atteint, label tenant ignoré"
            );
            return None;
        }
        // Incrémentation non-atomique avec le check ci-dessus — intentionnel : en cas de race
        // condition, quelques labels supplémentaires peuvent passer (at most N_threads au-dessus du cap).
        // C'est acceptable : le cap est une protection DoS soft, pas un hard limit cryptographique.
        self.tenant_count.fetch_add(1, Ordering::Relaxed);
        Some(label)
    }
}

impl Default for AppMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Handler /metrics
// ---------------------------------------------------------------------------

/// Axum handler for the `/metrics` endpoint (loopback side-channel).
///
/// Encodes the Prometheus registry in OpenMetrics text format.
/// Returns 500 if encoding fails (should not happen in practice).
pub async fn metrics_handler(State(m): State<AppMetrics>) -> Result<Response, StatusCode> {
    let mut buf = String::new();
    encode(&mut buf, &m.registry).map_err(|e| {
        tracing::error!(error = %e, "échec encodage métriques Prometheus");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Response::builder()
        .header(
            "Content-Type",
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )
        .body(Body::from(buf))
        .map_err(|e| {
            tracing::error!(error = %e, "échec construction réponse /metrics");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

// ---------------------------------------------------------------------------
// Listener loopback
// ---------------------------------------------------------------------------

/// Starts the metrics listener on `bind` (must be loopback — no TLS escape for metrics).
///
/// Spawned from `main.rs` after the main listener is bound.
///
/// # Errors
/// - Returns `Err` if `bind` is not loopback (metrics must not escape the loopback).
/// - Returns `Err` if the TCP bind fails or if `axum::serve` returns an error.
pub async fn spawn_metrics_listener(
    bind: std::net::SocketAddr,
    m: AppMetrics,
) -> anyhow::Result<()> {
    use axum::{routing::get, Router};

    if !bind.ip().is_loopback() {
        anyhow::bail!(
            "metrics listener doit être loopback (caveat C7) : adresse refusée = {}",
            bind
        );
    }

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(m);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(addr = %bind, "metrics listener en écoute");
    axum::serve(listener, app).await?;
    Ok(())
}
