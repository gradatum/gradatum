//! Prometheus metrics — canal latéral loopback (caveat C7).
//!
//! Lié exclusivement au loopback (127.0.0.1:19091 par défaut). Non configurable
//! sur une adresse non-loopback en Phase 2.0 (C7 strict : pas de TLS escape contrairement
//! au bind principal).
//!
//! La cardinalité des labels est plafonnée (défaut 100/série). Les labels sont sanitisés
//! via une whitelist statique — les paths utilisent des templates de routes, jamais des URI
//! concrètes (ex. `/api/v1/vault_search` et non `/api/v1/vault_search?q=secret`).
//!
//! # Métriques déclarées
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

/// Labels pour les requêtes HTTP — paths sanitisés (templates, jamais URIs concrètes).
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct HttpReqLabels {
    /// Méthode HTTP (GET, POST, …).
    pub method: String,
    /// Template de route (ex. `/api/v1/vault_search`).
    pub path: &'static str,
    /// Code HTTP réponse (200, 400, 500, …).
    pub status: u16,
}

/// Label tenant — contrôlé par cardinality cap.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct TenantLabel {
    pub tenant: String,
}

/// Label raison d'échec auth.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct AuthFailLabel {
    /// Raison d'échec (ex. `"invalid_token"`, `"expired"`, `"revoked"`).
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

/// Métriques applicatives exportées sur le canal latéral loopback :19091.
///
/// Clonable — le `Registry` et les familles sont wrappés dans `Arc`.
/// Injecté dans `AppState` (T11) et dans le routeur metrics séparé.
///
/// Les champs sont `pub` pour être accessibles par les middlewares HTTP (T12+) et les handlers.
/// `dead_code` supprimé : les champs sont des stubs intentionnels — branchement en P2.0b.
#[allow(dead_code)]
#[derive(Clone)]
pub struct AppMetrics {
    /// Registry Prometheus (partagé via Arc pour permettre le clonage).
    pub registry: Arc<Registry>,

    // -- Métriques HTTP -------------------------------------------------------
    /// Nombre total de requêtes HTTP (method, path template, status).
    pub http_requests: Family<HttpReqLabels, Counter>,
    /// Durée des requêtes HTTP en secondes (method, path template).
    pub http_duration: Family<HttpReqLabels, Histogram>,

    // -- File d'attente -------------------------------------------------------
    /// Profondeur de la file d'écriture par tenant (label contrôlé par cap).
    pub queue_depth: Family<TenantLabel, Gauge>,
    /// Décalage de la file d'écriture en secondes par tenant (label contrôlé par cap).
    pub queue_lag: Family<TenantLabel, Gauge>,

    // -- Auth -----------------------------------------------------------------
    /// Nombre d'échecs d'authentification par raison.
    pub auth_failures: Family<AuthFailLabel, Counter>,
    /// Taille du store de révocation (nombre d'entrées).
    pub revocation_size: Gauge,

    // -- Curator / LLM (stubs T11 — impl effective P2.0b) --------------------
    /// Décisions curator par action — stub T11.
    pub curator_decisions: Family<CuratorActionLabel, Counter>,
    /// Appels LLM backend par backend+outcome — stub T11.
    pub llm_calls: Family<LlmBackendLabel, Counter>,

    // -- Event-log (B1 tranche v0.3.0) ----------------------------------------
    /// Nombre de lignes courantes dans la table `event_log`.
    ///
    /// Mis à jour par la tâche de rétention tokio interval (6h).
    /// Ne pas appeler depuis les handlers (scan complet — lazy uniquement).
    pub event_log_rows: Gauge,

    // -- Cardinality cap (tenant) --------------------------------------------
    /// Nombre de labels tenant distincts enregistrés jusqu'ici.
    tenant_count: Arc<AtomicUsize>,
    /// Plafond de cardinalité par série tenant (défaut : 100).
    cap: usize,
}

impl AppMetrics {
    /// Crée et enregistre les 8 métriques dans un nouveau `Registry`.
    ///
    /// # Buckets histogram
    /// Buckets HTTP duration : 10 valeurs exponentielles à partir de 1ms (base 2),
    /// couvrant ~1ms – ~1s.
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

    /// Enregistre un label tenant, en appliquant le plafond de cardinalité.
    // Utilisé par les middlewares HTTP (T12+) et directement par les tests.
    #[allow(dead_code)]
    ///
    /// # Comportement
    /// - Si la cardinalité n'a pas encore atteint le plafond (`cap`), incrémente le compteur
    ///   et retourne `Some(label)` — l'appelant peut utiliser ce label pour observer des métriques.
    /// - Si le plafond est atteint, log un warning et retourne `None` — le label est ignoré.
    ///
    /// # Note importante
    /// Ce compteur est un compteur _d'admission_ : il comptabilise les labels uniques
    /// présentés pour la première fois. Il ne connaît pas les labels déjà créés dans les Family.
    /// Pour un usage correct : appeler `observe_tenant` une seule fois par tenant distinct,
    /// puis réutiliser le label directement pour les mises à jour ultérieures de métriques.
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

/// Handler Axum pour l'endpoint `/metrics` (canal latéral loopback).
///
/// Encode le registry Prometheus au format OpenMetrics text.
/// Retourne 500 si l'encodage échoue (ne devrait pas arriver en pratique).
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

/// Démarre le listener métriques sur `bind` (doit être loopback — caveat C7 strict).
///
/// Spawné depuis `main.rs` après que le listener principal est lié.
///
/// # Erreurs
/// - Retourne `Err` si `bind` n'est pas loopback (C7 : pas de TLS escape pour les métriques).
/// - Retourne `Err` si le bind TCP échoue ou si axum::serve retourne une erreur.
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
