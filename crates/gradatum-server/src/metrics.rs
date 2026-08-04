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
//! | `gradatum_http_request_duration_seconds` | Histogram | method, path (template) — no status |
//! | `gradatum_queue_depth` | Gauge | tenant |
//! | `gradatum_queue_lag_seconds` | Gauge | tenant |
//! | `gradatum_auth_failures_total` | Counter | reason |
//! | `gradatum_revocation_store_size` | Gauge | (sans label) |
//! | `gradatum_curator_decisions_total` | Counter | path, outcome — F-66 |
//! | `gradatum_llm_backend_calls_total` | Counter | backend, outcome — stub (not yet instrumented) |
//! | `gradatum_vault_context_duration_seconds` | Histogram | mode — since v0.7.0 |
//! | `gradatum_vault_context_embed_fallback_total` | Counter | mode — since v0.7.0 |
//! | `gradatum_vault_context_candidates` | Histogram | mode — since v0.7.0 |
//! | `gradatum_vault_context_included` | Histogram | mode — since v0.7.0 |
//! | `gradatum_vault_proactive_recall_surfaced_total` | Counter | mode — Active Recall, since v0.7.1 |
//! | `gradatum_vault_proactive_recall_accepted_total` | Counter | (no label) — Active Recall, since v0.7.1 |
//! | `gradatum_vault_proactive_refresh_total` | Counter | (no label) — Active Recall, since v0.7.1 |
//! | `gradatum_vault_proactive_recall_duration_seconds` | Histogram | mode — Active Recall, since v0.7.1 |
//! | `gradatum_vault_proactive_refresh_duration_seconds` | Histogram | (no label) — Active Recall, since v0.7.1 |
//! | `gradatum_context_inline_total` | Counter | mode (assembled\|compact) — since v0.7.2 |
//! | `gradatum_context_stub_total` | Counter | mode (assembled\|compact) — since v0.7.2 |
//! | `gradatum_context_dropped_total` | Counter | mode (assembled\|compact) — since v0.7.2 |
//! | `gradatum_context_compaction_total` | Counter | (no label) — since v0.7.2 |
//! | `gradatum_context_tokens_saved` | Histogram | (no label) — since v0.7.2 |
//! | `gradatum_vault_orphan_dirs` | Gauge | (sans label) — réconciliation registre↔disque au boot (L7 volet 2) |
//! | `gradatum_ann_deficit_partitions` | Gauge | (sans label) — gate de santé ANN au boot |
//! | `gradatum_api_key_orphan_owners` | Gauge | (sans label) — clés actives dont l'owner n'est déclaré par aucun consumer du preset (B6′b) |

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{body::Body, extract::State, http::StatusCode, response::Response};
use prometheus_client::{
    encoding::text::encode,
    metrics::{
        counter::Counter,
        family::Family,
        gauge::Gauge,
        histogram::{Histogram, exponential_buckets},
    },
    registry::Registry,
};

// ---------------------------------------------------------------------------
// Label sets
// ---------------------------------------------------------------------------

/// Labels for the HTTP request counter — sanitized paths (templates, never concrete URIs).
///
/// Populated by `crate::middleware::http_metrics_middleware` from the axum `MatchedPath`
/// extension. `path` is a `String` because the route pattern is owned by the router at
/// runtime, not a compile-time constant; its domain stays bounded by the route table
/// (see the middleware's `# Cardinality` section).
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct HttpReqLabels {
    /// HTTP method (GET, POST, …).
    pub method: String,
    /// Route template (e.g., `/api/v1/vault_search`), never a concrete URI.
    pub path: String,
    /// HTTP response status code (200, 400, 500, …).
    pub status: u16,
}

/// Labels for the HTTP duration histogram — same as [`HttpReqLabels`] **without** `status`.
///
/// Deliberately distinct: a latency distribution is read per route, not per status code,
/// and folding `status` in would multiply the number of histograms (10 buckets + sum +
/// count each) by the number of observed status codes for no operational gain.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct HttpDurLabels {
    /// HTTP method (GET, POST, …).
    pub method: String,
    /// Route template (e.g., `/api/v1/vault_search`), never a concrete URI.
    pub path: String,
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

/// Label set for the review auto-promote counter.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct FromStatusLabel {
    /// Status from which the note was promoted (`"staging"` or `"pending-review"`).
    pub from_status: &'static str,
}

/// Label for F-51 audit candidates by dedup category.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct AuditCategoryLabel {
    /// Dedup category (`"exact-duplicate"`, `"structural-junk"`, ...).
    pub category: String,
}

/// Labels for curator decisions (F-66 instrumentation): decision path × outcome.
///
/// Populated by `handle_persist_curated` from the `curator_decision` field of the
/// internal persist request. The worker owns the decision; the server owns the registry.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct CuratorDecisionLabel {
    /// Decision path: `hint_bypass` | `fast_admit` | `pending_band` | `llm_review`.
    pub path: String,
    /// Outcome: `admitted` | `pending` | `rejected`.
    pub outcome: String,
}

/// Labels for LLM backend calls (stub metric, not yet instrumented).
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct LlmBackendLabel {
    pub backend: &'static str,
    pub outcome: &'static str,
}

/// Label endpoint usage read-path — télémétrie feat/usage-telemetry-19091.
///
/// Valeurs canoniques : `/api/v1/vault_search`, `/api/v1/vault_read`,
/// `/api/v1/code_scope`, `/api/v1/vault_timeline`, `/api/v1/lessons/recall`.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct UsageEndpointLabel {
    /// Chemin de l'endpoint (ex. `/api/v1/vault_search`).
    pub endpoint: String,
}

/// Label outil MCP — télémétrie feat/usage-telemetry-19091.
///
/// Valeurs : noms des 21 outils MCP sans préfixe `mcp:` (ex. `vault_list`).
/// Le préfixe `mcp:` est retiré lors du fan-out dans `route_metric`.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct McpToolLabel {
    /// Nom de l'outil MCP (ex. `vault_list`, `vault_search`).
    pub tool: String,
}

/// Label de `kind` d'usage per-note (salience per note).
///
/// Cardinalité bornée à **5 valeurs** (vocabulaire fermé `note_usage_store::KIND_*`) :
/// `read`, `search-hit`, `search-hit-top3`, `recall-surfaced`, `recall-accepted`.
/// Sert de preuve de vie et de volumétrie globale (pas par note).
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct NoteUsageKindLabel {
    /// `kind` d'usage (borné, cf. `note_usage_store::KIND_*`). `String` par parité avec
    /// [`UsageEndpointLabel`] : les valeurs proviennent du vocabulaire fermé KIND_* →
    /// cardinalité 5 malgré le type ouvert.
    pub kind: String,
}

/// Labels pour l'assemblage de contexte vault_context (F-35 Context Assembly, v0.7.0).
///
/// Valeurs canoniques du champ `mode` : `"assembled"` (chemin nominal, embed OK),
/// `"fallback"` (embed échoué → repli BM25 pur).
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct ContextAssemblyLabel {
    /// Mode d'assemblage (ex. `"assembled"`, `"fallback"`).
    pub mode: &'static str,
}

/// Label de mode pour le rappel proactif (F-46 Active Recall, v0.7.1).
///
/// Cardinalité bornée à **2 valeurs** :
/// - `"proactive"` : lecture de surface pré-calculée (interval in-process B').
/// - `"contextual"` : retrieval RRF à la demande (contexte fourni par l'appelant).
///
/// Utilisé par [`AppMetrics::proactive_surfaced`] et
/// [`AppMetrics::proactive_recall_duration`].
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct ProactiveRecallModeLabel {
    /// Mode de recall : `"proactive"` ou `"contextual"` (borné, pas de cardinalité libre).
    pub mode: &'static str,
}

/// Label pour les détections de dérive d'écriture (F-36, v0.7.3).
///
/// Cardinalité bornée : le champ `rule` est `&'static str` issu de la table
/// `gradatum_core::write_check::TABLE` (13 catégories finies) — jamais un titre
/// ou un contenu dynamique (P2 council : zéro alloc hot-path).
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct DriftRuleLabel {
    /// Règle de dérive déclenchée (ex. `"category_section_coherence"`).
    pub rule: &'static str,
}

/// Label de mode pour les métriques context efficiency (F-29/F-30, v0.7.2).
///
/// Cardinalité bornée à **2 valeurs** :
/// - `"assembled"` : chemin nominal `assemble_assembled` (pipeline complet).
/// - `"compact"` : vue foldée `assemble_compact` (F-30 reset cache).
///
/// Utilisé par [`AppMetrics::context_inline_total`], [`AppMetrics::context_stub_total`]
/// et [`AppMetrics::context_dropped_total`].
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct ContextEfficiencyLabel {
    /// Mode d'assemblage : `"assembled"` ou `"compact"` (borné, pas de cardinalité libre).
    pub mode: &'static str,
}

/// Estimation du nombre de tokens économisés par stub produit dans la réponse.
///
/// Un stub remplace environ 200 tokens de corps de note médiane dans le prompt LLM.
/// Estimation déterministe documentée : notes médianes gradatum vault ≈ 150-250 tokens body.
/// Constante intentionnellement conservatrice — calibrable si besoin via bench futur.
/// Anti cache-bust : aucune dépendance au contenu de la note.
pub(crate) const AVG_STUB_TOKENS_SAVED: f64 = 200.0;

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
    ///
    /// Fed by `crate::middleware::http_metrics_middleware`, mounted as the outermost
    /// layer of the router — every route is counted, 5xx included.
    pub http_requests: Family<HttpReqLabels, Counter>,
    /// HTTP request duration in seconds (method, path template — no status).
    ///
    /// Same source as [`Self::http_requests`]; measures everything downstream of the
    /// metrics layer (auth, rate limiting, handler, serialisation).
    pub http_duration: Family<HttpDurLabels, Histogram>,

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

    // -- Curator / LLM -------------------------------------------------------
    /// Curator decisions by path × outcome (F-66 instrumentation).
    pub curator_decisions: Family<CuratorDecisionLabel, Counter>,
    /// LLM backend calls by backend+outcome — intentional stub (F-66 point ouvert).
    pub llm_calls: Family<LlmBackendLabel, Counter>,

    // -- Télémétrie usage (feat/usage-telemetry-19091) -------------------------
    /// Usage total par endpoint read-path.
    ///
    /// Série : `gradatum_read_usage_total{endpoint}`.
    /// Alimentée au boot (seed depuis DB) et à chaque flush 60s (delta).
    pub read_usage: Family<UsageEndpointLabel, Counter>,
    /// Appels outils MCP par outil.
    ///
    /// Série : `gradatum_mcp_tool_calls_total{tool}`.
    /// Alimentée au boot (seed depuis DB) et à chaque flush 60s (delta).
    pub mcp_tool_calls: Family<McpToolLabel, Counter>,

    /// Usage per-note agrégé par `kind` (salience per note).
    ///
    /// Série : `gradatum_note_usage_total{kind}`. Cardinalité 5. Alimentée au flush
    /// 60s (somme des deltas du batch par kind). Process-lifetime (pas de seed DB —
    /// preuve de vie/volumétrie, la donnée durable vit dans `note_usage`).
    pub note_usage_total: Family<NoteUsageKindLabel, Counter>,

    // -- Event-log (B1 tranche v0.3.0) ----------------------------------------
    /// Current row count in the `event_log` table.
    ///
    /// Updated by the tokio interval retention task (every 6h).
    /// Not to be called from handlers (full scan — lazy only).
    pub event_log_rows: Gauge,

    // -- Review auto-promote (review-promote job) ------------------------------
    /// Notes auto-promoted from review queue by the background job.
    /// Series: `gradatum_review_promoted_total{from_status}`.
    /// In-memory (resets on restart = normal for activity-job counters).
    pub review_promoted: Family<FromStatusLabel, Counter>,

    /// Errors during review auto-promotion (e.g. NoteNotFound TOCTOU).
    /// Series: `gradatum_review_promote_errors_total`.
    pub review_promote_errors: Counter,

    // -- F-51 audit / dedup pass (detection-only, Option A) --------------------
    /// Candidates found in the last audit pass, by dedup category.
    /// Series: `gradatum_audit_candidates{category}`. Set (not incremented) each pass.
    pub audit_candidates: Family<AuditCategoryLabel, Gauge>,
    /// Timestamp (epoch ms) of the last successful audit pass.
    /// Series: `gradatum_audit_last_run_timestamp_ms`.
    pub audit_last_run_ms: Gauge,
    /// Cumulative audit pass errors (scan/report write failures).
    /// Series: `gradatum_audit_errors_total`.
    pub audit_errors: Counter,
    /// Cumulative F-111 auto-downgrades performed by the audit executor.
    /// Series: `gradatum_audit_downgraded_total`.
    pub audit_downgraded: Counter,

    // -- vault_context télémétrie (F-35 Context Assembly, v0.7.0) ------------
    /// Durée d'assemblage vault_context par mode (secondes).
    /// Série : `gradatum_vault_context_duration_seconds{mode}`.
    /// Squelette câblé avant la logique métier (meta-plan §5).
    pub vault_context_duration: Family<ContextAssemblyLabel, Histogram>,
    /// Fallbacks BM25 sur échec embed dans vault_context par mode.
    /// Série : `gradatum_vault_context_embed_fallback_total{mode}`.
    pub vault_context_embed_fallback: Family<ContextAssemblyLabel, Counter>,
    /// Candidats considérés par vault_context avant filtrage par mode.
    /// Série : `gradatum_vault_context_candidates{mode}`.
    pub vault_context_candidates: Family<ContextAssemblyLabel, Histogram>,
    /// Notes incluses dans le contexte assemblé par mode.
    /// Série : `gradatum_vault_context_included{mode}`.
    pub vault_context_included: Family<ContextAssemblyLabel, Histogram>,

    // -- Proactive Recall (F-46 Active Recall, v0.7.1) -----------------------
    /// Hits surfacés par pull proactif, indexés par mode (`"proactive"` | `"contextual"`).
    ///
    /// Incrémenté du nombre d'items POST-filtrage ACL retournés à l'appelant.
    /// Série : `gradatum_vault_proactive_recall_surfaced_total{mode}`.
    pub proactive_surfaced: Family<ProactiveRecallModeLabel, Counter>,
    /// Hits acceptés par feedback utilisateur (toutes sessions, sans label mode).
    ///
    /// Incrémenté du nombre d'`accepted_ulids` validés lors d'un `proactive_recall_feedback`.
    /// Série : `gradatum_vault_proactive_recall_accepted_total`.
    pub proactive_accepted: Counter,
    /// Refreshs de surface proactive réussis (sorties `Ok(…)` de `proactive_refresh_once`).
    ///
    /// Série : `gradatum_vault_proactive_refresh_total`.
    pub proactive_refresh: Counter,
    /// Durée d'un pull proactif par mode (secondes).
    ///
    /// Observée inconditionnellement (même si surface vide) — utile pour le diagnostic latence.
    /// Série : `gradatum_vault_proactive_recall_duration_seconds{mode}`.
    pub proactive_recall_duration: Family<ProactiveRecallModeLabel, Histogram>,
    /// Durée d'un refresh de surface proactive (secondes).
    ///
    /// Observée sur chaque sortie `Ok(…)` de `proactive_refresh_once`.
    /// Série : `gradatum_vault_proactive_refresh_duration_seconds`.
    pub proactive_refresh_duration: Histogram,

    // -- Context Efficiency (F-29/F-30, v0.7.2) ------------------------------
    /// Notes inline dans la réponse context, par mode (`"assembled"` | `"compact"`).
    ///
    /// Incrémenté du nombre de notes retournées en corps complet (budget inline).
    /// Série : `gradatum_context_inline_total{mode}`.
    pub context_inline_total: Family<ContextEfficiencyLabel, Counter>,
    /// Stubs produits dans la réponse context, par mode (`"assembled"` | `"compact"`).
    ///
    /// Incremented by the number of stubs exposed in `references`.
    /// Série : `gradatum_context_stub_total{mode}`.
    pub context_stub_total: Family<ContextEfficiencyLabel, Counter>,
    /// Notes droppées (hors budget inline et stub), par mode.
    ///
    /// `inline + stub + dropped == candidates_considered` (invariant de cohérence).
    /// Série : `gradatum_context_dropped_total{mode}`.
    pub context_dropped_total: Family<ContextEfficiencyLabel, Counter>,
    /// Compact mode calls (folded views) — absolute counter, no label.
    ///
    /// Incrémenté à chaque appel nominal à `assemble_compact` (pas les early-returns vides).
    /// Série : `gradatum_context_compaction_total`.
    pub context_compaction_total: Counter,
    /// Tokens économisés estimés par les stubs produits (stub_count × `AVG_STUB_TOKENS_SAVED`).
    ///
    /// Histogramme sans label : agrège les deux modes (assembled + compact).
    /// Buckets : 0–3200 tokens (0 à ~16 stubs × 200 tokens/stub).
    /// Série : `gradatum_context_tokens_saved`.
    pub context_tokens_saved: Histogram,

    // -- Write drift detection (F-36, v0.7.3) ---------------------------------
    /// Violations de dérive d'écriture par règle (warn-only, ne bloque jamais le write).
    ///
    /// Incrémenté dans `vault_write_impl` après l'ACL, avant l'enqueue.
    /// Série : `gradatum_write_check_total{rule}`.
    pub write_check: Family<DriftRuleLabel, Counter>,

    // -- Réconciliation registre↔disque au boot (L7 volet 2) -------------------
    /// Répertoires de vault présents sur disque mais **absents du registre** (aucune ligne
    /// `tenants`, quel que soit le statut) — détectés à chaque boot à flag `multi_tenant` ON.
    ///
    /// Série : `gradatum_vault_orphan_dirs`. **Gauge** (état, pas flux) : la valeur est
    /// (re)positionnée à chaque boot — un orphelin nettoyé fait retomber la jauge sans
    /// redémarrage du compteur. Volet 2 est **non bloquant** : la jauge et un `warn` sont
    /// l'unique effet, le boot continue.
    ///
    /// À flag OFF (défaut LIVE) la réconciliation n'est jamais exécutée : la jauge reste à
    /// `0` (valeur d'initialisation), aucune I/O disque n'est faite.
    pub vault_orphan_dirs: Gauge,

    // -- Gate de santé ANN au boot --------------------------------------------
    /// Partitions ANN `(vault_id, embedder_id)` dont l'index dérivé est **incomplet**
    /// (`note_embeddings_ann` compte moins de lignes que les paires éligibles de
    /// `note_embeddings`) — mesuré à chaque boot, après backfill et GC.
    ///
    /// Série : `gradatum_ann_deficit_partitions`. **Gauge** (état, pas flux) : la valeur est
    /// (re)positionnée à chaque boot. Une valeur `> 0` signifie que le gate a **fermé le
    /// chemin ANN** : la recherche sémantique est servie en brute-force (exacte, plus lente)
    /// jusqu'à ce qu'un redémarrage reconstruise l'index et remesure.
    ///
    /// Sans label : le nom des partitions fautives vit dans les `error!` du boot, pas dans
    /// une série Prometheus — `(vault × embedder)` est une cardinalité non bornée.
    ///
    /// À ANN OFF (défaut LIVE) le gate n'est jamais exécuté : la jauge reste à `0` (valeur
    /// d'initialisation) et aucune requête n'est émise.
    pub ann_deficit_partitions: Gauge,

    // -- Intégrité référentielle owner ↔ identité ACL au boot (B6′b) ----------
    /// Clés API **actives** dont l'`owner` n'est déclaré par aucun `[[consumer]]` du preset
    /// ACL chargé — mesuré à chaque boot.
    ///
    /// Série : `gradatum_api_key_orphan_owners`. **Gauge** (état, pas flux) : la valeur est
    /// (re)positionnée à chaque boot — déclarer l'identité manquante, ou révoquer la clé,
    /// fait retomber la jauge au démarrage suivant.
    ///
    /// Une telle clé s'authentifie puis se fait refuser sur tous les locus : le symptôme
    /// observé est une panne, la cause est une donnée. C'est le seul état que cette jauge
    /// nomme, et **jamais** un motif de refuser le démarrage — un service qui ne démarre pas
    /// sur une donnée héritée est un incident pire que celui qu'il signale.
    ///
    /// Sans label : le nom des owners fautifs vit dans les `error!` du boot, pas dans une
    /// série Prometheus — `owner` est une cardinalité non bornée.
    pub api_key_orphan_owners: Gauge,

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
        let http_duration: Family<HttpDurLabels, Histogram> =
            Family::new_with_constructor(|| Histogram::new(exponential_buckets(0.001, 2.0, 10)));
        let queue_depth: Family<TenantLabel, Gauge> = Family::default();
        let queue_lag: Family<TenantLabel, Gauge> = Family::default();
        let auth_failures: Family<AuthFailLabel, Counter> = Family::default();
        let revocation_size: Gauge = Gauge::default();
        let curator_decisions: Family<CuratorDecisionLabel, Counter> = Family::default();
        let llm_calls: Family<LlmBackendLabel, Counter> = Family::default();
        let read_usage: Family<UsageEndpointLabel, Counter> = Family::default();
        let mcp_tool_calls: Family<McpToolLabel, Counter> = Family::default();
        let note_usage_total: Family<NoteUsageKindLabel, Counter> = Family::default();
        let event_log_rows: Gauge = Gauge::default();
        let review_promoted: Family<FromStatusLabel, Counter> = Family::default();
        let review_promote_errors: Counter = Counter::default();
        let audit_candidates: Family<AuditCategoryLabel, Gauge> = Family::default();
        let audit_last_run_ms: Gauge = Gauge::default();
        let audit_errors: Counter = Counter::default();
        let audit_downgraded: Counter = Counter::default();
        let vault_context_duration: Family<ContextAssemblyLabel, Histogram> =
            Family::new_with_constructor(|| Histogram::new(exponential_buckets(0.001, 2.0, 10)));
        let vault_context_embed_fallback: Family<ContextAssemblyLabel, Counter> = Family::default();
        let vault_context_candidates: Family<ContextAssemblyLabel, Histogram> =
            Family::new_with_constructor(|| {
                Histogram::new([1.0_f64, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0].into_iter())
            });
        let vault_context_included: Family<ContextAssemblyLabel, Histogram> =
            Family::new_with_constructor(|| {
                Histogram::new([1.0_f64, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0].into_iter())
            });

        // -- Proactive Recall (F-46 Active Recall, v0.7.1) -------------------
        let proactive_surfaced: Family<ProactiveRecallModeLabel, Counter> = Family::default();
        let proactive_accepted: Counter = Counter::default();
        let proactive_refresh: Counter = Counter::default();
        let proactive_recall_duration: Family<ProactiveRecallModeLabel, Histogram> =
            Family::new_with_constructor(|| Histogram::new(exponential_buckets(0.001, 2.0, 10)));
        let proactive_refresh_duration: Histogram =
            Histogram::new(exponential_buckets(0.001, 2.0, 10));

        // -- Context Efficiency (F-29/F-30, v0.7.2) --------------------------
        let context_inline_total: Family<ContextEfficiencyLabel, Counter> = Family::default();
        let context_stub_total: Family<ContextEfficiencyLabel, Counter> = Family::default();
        let context_dropped_total: Family<ContextEfficiencyLabel, Counter> = Family::default();
        let context_compaction_total: Counter = Counter::default();
        // Buckets : couvrent 0–3 200 tokens économisés (0 à ~16 stubs × 200 tokens/stub).
        let context_tokens_saved: Histogram = Histogram::new(
            [
                0.0_f64, 200.0, 400.0, 600.0, 800.0, 1_200.0, 1_600.0, 3_200.0,
            ]
            .into_iter(),
        );

        // -- Write drift detection (F-36, v0.7.3) ----------------------------
        let write_check: Family<DriftRuleLabel, Counter> = Family::default();

        // -- Réconciliation registre↔disque au boot (L7 volet 2) -------------
        let vault_orphan_dirs: Gauge = Gauge::default();

        // -- Gate de santé ANN au boot ---------------------------------------
        let ann_deficit_partitions: Gauge = Gauge::default();

        // -- Intégrité référentielle owner ↔ identité ACL au boot (B6′b) ------
        let api_key_orphan_owners: Gauge = Gauge::default();

        let mut registry = Registry::default();

        registry.register(
            "gradatum_http_requests",
            "Total number of HTTP requests received",
            http_requests.clone(),
        );
        registry.register(
            "gradatum_http_request_duration_seconds",
            "HTTP request duration in seconds",
            http_duration.clone(),
        );
        registry.register(
            "gradatum_queue_depth",
            "Write queue depth per tenant",
            queue_depth.clone(),
        );
        registry.register(
            "gradatum_queue_lag_seconds",
            "Write queue lag in seconds per tenant",
            queue_lag.clone(),
        );
        registry.register(
            "gradatum_auth_failures",
            "Number of authentication failures by reason",
            auth_failures.clone(),
        );
        registry.register(
            "gradatum_revocation_store_size",
            "Number of entries in the revocation store",
            revocation_size.clone(),
        );
        registry.register(
            "gradatum_curator_decisions",
            "Curator decisions by decision path and outcome (F-66)",
            curator_decisions.clone(),
        );
        registry.register(
            "gradatum_llm_backend_calls",
            "LLM backend calls by backend+outcome (stub T11)",
            llm_calls.clone(),
        );
        registry.register(
            "gradatum_event_log_rows",
            "Current number of rows in event_log (updated by the retention task)",
            event_log_rows.clone(),
        );
        registry.register(
            "gradatum_read_usage",
            "Total number of invocations per read-path endpoint (durable — seeded from DB at boot)",
            read_usage.clone(),
        );
        registry.register(
            "gradatum_mcp_tool_calls",
            "Total number of MCP tool calls per tool (durable — seeded from DB at boot)",
            mcp_tool_calls.clone(),
        );
        registry.register(
            "gradatum_note_usage",
            "Per-note usage events aggregated by kind (F-110 salience, process-lifetime)",
            note_usage_total.clone(),
        );
        registry.register(
            "gradatum_review_promoted",
            "Notes auto-promoted from the review queue (by from_status)",
            review_promoted.clone(),
        );
        registry.register(
            "gradatum_review_promote_errors",
            "Errors during review auto-promotion (e.g. NoteNotFound TOCTOU)",
            review_promote_errors.clone(),
        );
        registry.register(
            "gradatum_audit_candidates",
            "F-51 audit candidates found in the last pass, by dedup category",
            audit_candidates.clone(),
        );
        registry.register(
            "gradatum_audit_last_run_timestamp_ms",
            "Epoch-ms timestamp of the last successful F-51 audit pass",
            audit_last_run_ms.clone(),
        );
        registry.register(
            "gradatum_audit_errors",
            "Cumulative F-51 audit pass errors (scan or report-write failures)",
            audit_errors.clone(),
        );
        registry.register(
            "gradatum_audit_downgraded",
            "Cumulative F-111 auto-downgrades performed by the audit executor",
            audit_downgraded.clone(),
        );
        registry.register(
            "gradatum_vault_context_duration_seconds",
            "vault_context duration by mode (skeleton F-35, v0.7.0)",
            vault_context_duration.clone(),
        );
        registry.register(
            "gradatum_vault_context_embed_fallback",
            "BM25 fallback on embed failure in vault_context by mode (skeleton F-35, v0.7.0)",
            vault_context_embed_fallback.clone(),
        );
        registry.register(
            "gradatum_vault_context_candidates",
            "Candidates considered by vault_context before filtering, by mode (skeleton F-35, v0.7.0)",
            vault_context_candidates.clone(),
        );
        registry.register(
            "gradatum_vault_context_included",
            "Notes included in the assembled context by mode (skeleton F-35, v0.7.0)",
            vault_context_included.clone(),
        );
        // -- Proactive Recall (F-46 Active Recall, v0.7.1) -------------------
        registry.register(
            "gradatum_vault_proactive_recall_surfaced",
            "Hits surfaced by proactive pull by mode (F-46 Active Recall, v0.7.1)",
            proactive_surfaced.clone(),
        );
        registry.register(
            "gradatum_vault_proactive_recall_accepted",
            "Hits accepted via user feedback (F-46 Active Recall, v0.7.1)",
            proactive_accepted.clone(),
        );
        registry.register(
            "gradatum_vault_proactive_refresh",
            "Successful proactive surface refreshes (F-46 Active Recall, v0.7.1)",
            proactive_refresh.clone(),
        );
        registry.register(
            "gradatum_vault_proactive_recall_duration_seconds",
            "Proactive pull duration by mode in seconds (F-46 Active Recall, v0.7.1)",
            proactive_recall_duration.clone(),
        );
        registry.register(
            "gradatum_vault_proactive_refresh_duration_seconds",
            "Proactive surface refresh duration in seconds (F-46 Active Recall, v0.7.1)",
            proactive_refresh_duration.clone(),
        );
        // -- Context Efficiency (F-29/F-30, v0.7.2) --------------------------
        registry.register(
            "gradatum_context_inline",
            "Notes returned inline (full body) by context mode (F-29/F-30, v0.7.2)",
            context_inline_total.clone(),
        );
        registry.register(
            "gradatum_context_stub",
            "Stubs produced in the context response by mode (F-29/F-30, v0.7.2)",
            context_stub_total.clone(),
        );
        registry.register(
            "gradatum_context_dropped",
            "Notes dropped over budget in context by mode (F-29/F-30, v0.7.2)",
            context_dropped_total.clone(),
        );
        registry.register(
            "gradatum_context_compaction",
            "Compact-mode calls (folded views F-30, v0.7.2)",
            context_compaction_total.clone(),
        );
        registry.register(
            "gradatum_context_tokens_saved",
            "Estimated tokens saved by context stubs (stub_count × 200, F-29/F-30, v0.7.2)",
            context_tokens_saved.clone(),
        );
        // -- Write drift detection (F-36, v0.7.3) ----------------------------
        registry.register(
            "gradatum_write_check",
            "Write-drift detections by rule (F-36 warn-only, v0.7.3)",
            write_check.clone(),
        );
        // -- Réconciliation registre↔disque au boot (L7 volet 2) -------------
        // Enregistrée INCONDITIONNELLEMENT (comme toutes les autres familles) : la série
        // existe donc dès le boot, à `0`, même à flag OFF. Une jauge qui n'apparaîtrait
        // qu'à flag ON serait indistinguable de « scrape cassé » côté Prometheus.
        registry.register(
            "gradatum_vault_orphan_dirs",
            "Vault directories present on disk but absent from the tenants registry (boot reconciliation, L7)",
            vault_orphan_dirs.clone(),
        );
        // -- Gate de santé ANN au boot ---------------------------------------
        // Enregistrée INCONDITIONNELLEMENT, comme les autres : la série existe dès le boot,
        // à `0`, même à ANN OFF. Une jauge qui n'apparaîtrait qu'à ANN ON serait
        // indistinguable d'un scrape cassé côté Prometheus.
        registry.register(
            "gradatum_ann_deficit_partitions",
            "ANN partitions (vault, embedder) whose derived index is incomplete — > 0 means the boot gate closed the ANN path (brute-force in effect)",
            ann_deficit_partitions.clone(),
        );
        // -- Intégrité référentielle owner ↔ identité ACL au boot (B6′b) ------
        // Enregistrée INCONDITIONNELLEMENT : la série existe dès le boot, à `0`. C'est ce
        // qui rend « 0 orphelin » lisible — sans elle, l'absence de série et l'absence
        // d'orphelin auraient la même apparence côté Prometheus.
        registry.register(
            "gradatum_api_key_orphan_owners",
            "Active API keys whose owner is declared by no consumer of the loaded ACL preset (boot reconciliation) — such keys authenticate then get denied everywhere",
            api_key_orphan_owners.clone(),
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
            read_usage,
            mcp_tool_calls,
            note_usage_total,
            event_log_rows,
            review_promoted,
            review_promote_errors,
            audit_candidates,
            audit_last_run_ms,
            audit_errors,
            audit_downgraded,
            vault_context_duration,
            vault_context_embed_fallback,
            vault_context_candidates,
            vault_context_included,
            proactive_surfaced,
            proactive_accepted,
            proactive_refresh,
            proactive_recall_duration,
            proactive_refresh_duration,
            context_inline_total,
            context_stub_total,
            context_dropped_total,
            context_compaction_total,
            context_tokens_saved,
            write_check,
            vault_orphan_dirs,
            ann_deficit_partitions,
            api_key_orphan_owners,
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
                "cardinality cap reached, tenant label ignored"
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
        tracing::error!(error = %e, "Prometheus metrics encoding failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Response::builder()
        .header(
            "Content-Type",
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )
        .body(Body::from(buf))
        .map_err(|e| {
            tracing::error!(error = %e, "/metrics response construction failed");
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
    use axum::{Router, routing::get};

    if !bind.ip().is_loopback() {
        anyhow::bail!(
            "metrics listener must be loopback (caveat C7): address rejected = {}",
            bind
        );
    }

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(m);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(addr = %bind, "metrics listener listening");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Les deux familles sont enregistrées et encodées avec les bons noms `_total`.
    ///
    /// Verifies that `gradatum_read_usage_total` + `gradatum_mcp_tool_calls_total`
    /// appear in the Prometheus encoding after an `inc_by`.
    #[test]
    fn metrics_expose_read_usage_and_mcp_tool_calls() {
        let m = AppMetrics::new();
        m.read_usage
            .get_or_create(&UsageEndpointLabel {
                endpoint: "/api/v1/vault_search".into(),
            })
            .inc_by(3);
        m.mcp_tool_calls
            .get_or_create(&McpToolLabel {
                tool: "vault_list".into(),
            })
            .inc_by(5);
        let mut buf = String::new();
        encode(&mut buf, &m.registry).unwrap();
        assert!(
            buf.contains("gradatum_read_usage_total"),
            "gradatum_read_usage_total doit apparaître dans l'encodage"
        );
        assert!(
            buf.contains("endpoint=\"/api/v1/vault_search\""),
            "label endpoint doit apparaître"
        );
        assert!(
            buf.contains("gradatum_mcp_tool_calls_total"),
            "gradatum_mcp_tool_calls_total doit apparaître dans l'encodage"
        );
        assert!(
            buf.contains("tool=\"vault_list\""),
            "label tool doit apparaître"
        );
    }

    /// Verifies that `gradatum_review_promoted_total{from_status}` + `gradatum_review_promote_errors_total`
    /// appear in the Prometheus encoding after inc.
    #[test]
    fn metrics_expose_review_promoted() {
        let m = AppMetrics::new();
        m.review_promoted
            .get_or_create(&FromStatusLabel {
                from_status: "staging",
            })
            .inc_by(3);
        m.review_promote_errors.inc();
        let mut buf = String::new();
        encode(&mut buf, &m.registry).unwrap();
        assert!(
            buf.contains("gradatum_review_promoted_total"),
            "gradatum_review_promoted_total doit apparaître dans l'encodage"
        );
        assert!(
            buf.contains("from_status=\"staging\""),
            "label from_status=\"staging\" doit apparaître"
        );
        assert!(
            buf.contains("gradatum_review_promote_errors_total"),
            "gradatum_review_promote_errors_total doit apparaître dans l'encodage"
        );
    }

    /// F-66 : `gradatum_curator_decisions_total{path, outcome}` s'encode par
    /// (path, outcome) distinct, chaque paire étant une série indépendante.
    #[test]
    fn metrics_expose_curator_decisions_by_path_outcome() {
        let m = AppMetrics::new();
        m.curator_decisions
            .get_or_create(&CuratorDecisionLabel {
                path: "fast_admit".to_string(),
                outcome: "admitted".to_string(),
            })
            .inc();
        m.curator_decisions
            .get_or_create(&CuratorDecisionLabel {
                path: "pending_band".to_string(),
                outcome: "pending".to_string(),
            })
            .inc();
        // Même paire deux fois → une seule série à 2 (pas de double-série).
        m.curator_decisions
            .get_or_create(&CuratorDecisionLabel {
                path: "fast_admit".to_string(),
                outcome: "admitted".to_string(),
            })
            .inc();

        let mut buf = String::new();
        encode(&mut buf, &m.registry).unwrap();
        assert!(
            buf.contains("gradatum_curator_decisions_total"),
            "série curator_decisions absente de l'encodage"
        );
        assert!(
            buf.contains("path=\"fast_admit\"") && buf.contains("outcome=\"admitted\""),
            "labels fast_admit/admitted absents"
        );
        assert!(
            buf.contains("path=\"pending_band\"") && buf.contains("outcome=\"pending\""),
            "labels pending_band/pending absents"
        );
        assert!(
            buf.contains("path=\"fast_admit\",outcome=\"admitted\"} 2"),
            "la paire répétée doit s'agréger à 2 (pas de double-comptage de série) — buf:\n{buf}"
        );
    }
}

#[cfg(test)]
mod tests_v070 {
    use super::*;

    /// vault_context_embed_fallback est enregistré et encodé avec le bon nom de série.
    ///
    /// Verifies the 4 vault_context metric families (duration, embed_fallback,
    /// candidates, included) are registered in AppMetrics (since v0.7.0).
    #[test]
    fn vault_context_metrics_register_and_increment() {
        let m = AppMetrics::new();
        m.vault_context_embed_fallback
            .get_or_create(&ContextAssemblyLabel { mode: "assembled" })
            .inc();
        // P3-2 : registry est Arc<Registry> → .as_ref() pour encoder
        let mut buf = String::new();
        encode(&mut buf, m.registry.as_ref()).unwrap();
        assert!(
            buf.contains("gradatum_vault_context_embed_fallback"),
            "gradatum_vault_context_embed_fallback doit apparaître dans l'encodage"
        );
    }
}

#[cfg(test)]
mod tests_v072 {
    use super::*;

    /// Verifies the 5 context efficiency metric families (since v0.7.2) are registered
    /// and encoded with exact series names (prefix `gradatum_context_`, `_total` for counters).
    ///
    /// Same pattern as `tests_v071::proactive_recall_metrics_register_and_encode`.
    #[test]
    fn context_efficiency_metrics_register_and_encode() {
        let m = AppMetrics::new();

        // Counters inline/stub/dropped avec label mode (assembled)
        m.context_inline_total
            .get_or_create(&ContextEfficiencyLabel { mode: "assembled" })
            .inc_by(5);
        m.context_stub_total
            .get_or_create(&ContextEfficiencyLabel { mode: "assembled" })
            .inc_by(3);
        m.context_dropped_total
            .get_or_create(&ContextEfficiencyLabel { mode: "assembled" })
            .inc_by(2);

        // Counter compaction (compact-only, sans label)
        m.context_compaction_total.inc();

        // Counters avec label mode compact (vérification cardinalité bornée)
        m.context_inline_total
            .get_or_create(&ContextEfficiencyLabel { mode: "compact" })
            .inc_by(4);
        m.context_stub_total
            .get_or_create(&ContextEfficiencyLabel { mode: "compact" })
            .inc_by(6);

        // Histogramme tokens_saved (3 stubs × 200 tokens/stub = 600)
        m.context_tokens_saved.observe(3.0 * AVG_STUB_TOKENS_SAVED);

        let mut buf = String::new();
        encode(&mut buf, m.registry.as_ref()).unwrap();

        assert!(
            buf.contains("gradatum_context_inline_total"),
            "context_inline_total manquant dans l'encodage"
        );
        assert!(
            buf.contains("mode=\"assembled\""),
            "label mode=\"assembled\" doit apparaître dans context_inline"
        );
        assert!(
            buf.contains("mode=\"compact\""),
            "label mode=\"compact\" doit apparaître dans context_stub"
        );
        assert!(
            buf.contains("gradatum_context_stub_total"),
            "context_stub_total manquant dans l'encodage"
        );
        assert!(
            buf.contains("gradatum_context_dropped_total"),
            "context_dropped_total manquant dans l'encodage"
        );
        assert!(
            buf.contains("gradatum_context_compaction_total"),
            "context_compaction_total manquant dans l'encodage"
        );
        assert!(
            buf.contains("gradatum_context_tokens_saved"),
            "context_tokens_saved manquant dans l'encodage"
        );
    }
}

#[cfg(test)]
mod tests_v071 {
    use super::*;

    /// Verifies the 5 proactive recall metric families (since v0.7.1) are registered
    /// and encoded with exact series names (prefix `gradatum_`, `_total` for counters,
    /// `_seconds` for duration histograms).
    ///
    /// Same pattern as `tests_v070::vault_context_metrics_register_and_increment`.
    #[test]
    fn proactive_recall_metrics_register_and_encode() {
        let m = AppMetrics::new();

        // Counter surfaced (avec label mode — 2 valeurs bornées)
        m.proactive_surfaced
            .get_or_create(&ProactiveRecallModeLabel { mode: "proactive" })
            .inc_by(3);

        // Counter accepted (sans label)
        m.proactive_accepted.inc_by(2);

        // Counter refresh (sans label)
        m.proactive_refresh.inc();

        // Histogramme durée pull (avec label mode)
        m.proactive_recall_duration
            .get_or_create(&ProactiveRecallModeLabel { mode: "contextual" })
            .observe(0.042);

        // Histogramme durée refresh (sans label)
        m.proactive_refresh_duration.observe(0.150);

        let mut buf = String::new();
        encode(&mut buf, m.registry.as_ref()).unwrap();

        assert!(
            buf.contains("gradatum_vault_proactive_recall_surfaced_total"),
            "surfaced_total manquant dans l'encodage (got: ...)"
        );
        assert!(
            buf.contains("mode=\"proactive\""),
            "label mode=\"proactive\" doit apparaître dans surfaced"
        );
        assert!(
            buf.contains("gradatum_vault_proactive_recall_accepted_total"),
            "accepted_total manquant dans l'encodage"
        );
        assert!(
            buf.contains("gradatum_vault_proactive_refresh_total"),
            "refresh_total manquant dans l'encodage"
        );
        assert!(
            buf.contains("gradatum_vault_proactive_recall_duration_seconds"),
            "recall_duration_seconds manquant dans l'encodage"
        );
        assert!(
            buf.contains("mode=\"contextual\""),
            "label mode=\"contextual\" doit apparaître dans recall_duration"
        );
        assert!(
            buf.contains("gradatum_vault_proactive_refresh_duration_seconds"),
            "refresh_duration_seconds manquant dans l'encodage"
        );
    }
}

#[cfg(test)]
mod tests_v073 {
    use super::*;

    /// Verifies that `gradatum_write_check_total{rule}` (since v0.7.3) is registered
    /// and encoded with the correct series name after an `inc()`.
    ///
    /// Same pattern as `tests_v072::context_efficiency_metrics_register_and_encode`.
    #[test]
    fn write_check_metric_registers_and_encodes() {
        let m = AppMetrics::new();

        // Incrémenter le compteur pour la règle `category_section_coherence`.
        m.write_check
            .get_or_create(&DriftRuleLabel {
                rule: "category_section_coherence",
            })
            .inc();

        let mut buf = String::new();
        encode(&mut buf, m.registry.as_ref()).unwrap();

        assert!(
            buf.contains("gradatum_write_check_total"),
            "gradatum_write_check_total doit apparaître dans l'encodage"
        );
        assert!(
            buf.contains("rule=\"category_section_coherence\""),
            "label rule=\"category_section_coherence\" doit apparaître"
        );
    }
}
