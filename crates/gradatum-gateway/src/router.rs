//! Reasoning router — decides THINK / NO_THINK for a chat request.
//!
//! Two tiers, cheapest first:
//! 1. **Pre-classifier** (`preclassify`): a deterministic, allocation-light check that
//!    resolves the obvious cases (empty query, greetings/acks, short system commands)
//!    WITHOUT calling the curator. It is intentionally conservative — it only emits
//!    `NO_THINK` when confident, because a missed THINK is costlier than a wasted one
//!    (design tie-break). Anything with a reasoning-trigger word is deferred to the curator.
//! 2. **Curator LLM** (`RouterClient::call_curator`): only the boundary cases reach the
//!    curator (`:18083`), GBNF-constrained to `THINK` / `NO_THINK`, `temperature = 0`.
//!
//! Robustness: a hard per-decision timeout and a concurrency semaphore isolate the router
//! from the enrich workload. On saturation / timeout / any error the router **fails fast to
//! the no-think fallback** — logged, never silent (ctx-gating invariant, council 01KWVXAWB3).
//! The definitive slot/parallel sizing comes from Bob at cutover; the `max_concurrent` cap
//! here is a configurable, safe default.
//!
//! Observability: the decision, its source and its latency are emitted as structured logs
//! AND as Prometheus series on `/metrics` — `gateway_router_decisions_total{source}`,
//! `gateway_router_fallback_total{reason}`, and the `gateway_router_curator_latency_seconds`
//! / `gateway_router_system_latency_seconds` histograms (probe D-9). The fallback counter is
//! incremented at the same point as the WARN below (never silent — invariant 01KWVXAWB3).

use std::fmt;
use std::time::{Duration, Instant};

use reqwest::Client;
use tokio::sync::Semaphore;

use crate::config::RouterConfig;
use crate::smart_router::ReasoningSource;

/// Router prompt artifacts, embedded at compile time (versioned with the binary — no
/// runtime file dependency).
const ROUTER_SYSTEM_PROMPT: &str = include_str!("../assets/router-system-prompt.txt");
const ROUTER_GRAMMAR: &str = include_str!("../assets/router.gbnf");

/// Reasoning-trigger words that veto a `NO_THINK` pre-classification: their presence
/// forces the boundary path (curator) regardless of surface form.
const REASONING_TRIGGERS: &[&str] = &[
    "why",
    "compare",
    "analyz",
    "analyse",
    "design",
    "plan",
    "debug",
    "explain",
    "diagnos",
    "prove",
    "derive",
    "optimi",
    "migrat",
    "architect",
    "tradeoff",
    "versus",
];

/// Exact greetings / acknowledgements → `NO_THINK` (nothing to reason about).
const GREETINGS: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "yo",
    "thanks",
    "thank you",
    "ok",
    "okay",
    "yes",
    "no",
    "bonjour",
    "merci",
    "salut",
    "hi there",
    "hello there",
];

/// First-word system-command verbs → `NO_THINK` when the query is short and trigger-free.
const COMMAND_VERBS: &[&str] = &[
    "restart", "start", "stop", "reboot", "kill", "enable", "disable", "status", "ping", "ls",
    "cat",
];

/// Deterministic cheap pre-classifier for the **reasoning** axis.
///
/// Returns `Some(false)` for a high-confidence NO_THINK request, `None` at the boundary
/// (defer to the curator). Vision is a separate axis and is NOT consulted here.
///
/// Conservative by design: only the unambiguous cases short-circuit the curator, so the
/// risk of a false NO_THINK (a missed reasoning pass) stays near zero. Broadening the
/// rules requires the B2 eval harness.
#[must_use]
pub(crate) fn preclassify(query: &str) -> Option<bool> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return Some(false);
    }
    if GREETINGS.contains(&q.as_str()) {
        return Some(false);
    }
    // Any reasoning-trigger word → boundary (protects recall-THINK).
    if REASONING_TRIGGERS.iter().any(|t| q.contains(t)) {
        return None;
    }
    // Short imperative system command (first word is a known verb) → NO_THINK.
    let words: Vec<&str> = q.split_whitespace().collect();
    if words.len() <= 6 && words.first().is_some_and(|w| COMMAND_VERBS.contains(w)) {
        return Some(false);
    }
    None
}

/// Parses a curator label into a reasoning decision.
///
/// `"THINK"` → `Some(true)`, `"NO_THINK"` → `Some(false)`, anything else → `None`
/// (GBNF constrains the output, but we validate defensively). A leading space
/// (BPE artefact tolerated by the grammar) is trimmed.
#[must_use]
pub(crate) fn parse_label(raw: &str) -> Option<bool> {
    match raw.trim() {
        "THINK" => Some(true),
        "NO_THINK" => Some(false),
        _ => None,
    }
}

/// Internal router error — every variant maps to the no-think fallback.
#[derive(Debug)]
enum RouterError {
    /// All concurrency permits in use (isolation cap reached).
    Saturated,
    /// Hard per-decision timeout elapsed.
    Timeout,
    /// Transport error contacting the curator.
    Http(reqwest::Error),
    /// Curator returned a non-success HTTP status.
    BadStatus(u16),
    /// Response body could not be parsed into a label.
    Unparseable,
}

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Saturated => write!(f, "router saturated (max_concurrent reached)"),
            Self::Timeout => write!(f, "timeout curator"),
            Self::Http(e) => write!(f, "curator transport error: {e}"),
            Self::BadStatus(s) => write!(f, "curator HTTP status {s}"),
            Self::Unparseable => write!(f, "unparsable curator response"),
        }
    }
}

impl RouterError {
    /// Stable, bounded label for the `gateway_router_fallback_total{reason}` metric.
    ///
    /// Collapses the 5 variants onto the 4 canonical reasons: a non-2xx status
    /// (`BadStatus`) is an HTTP-layer failure → `"http"` (same family as a transport
    /// error), and `Unparseable` → `"parse"`.
    fn reason_label(&self) -> &'static str {
        match self {
            Self::Saturated => "saturated",
            Self::Timeout => "timeout",
            Self::Http(_) | Self::BadStatus(_) => "http",
            Self::Unparseable => "parse",
        }
    }
}

/// Reasoning router client — pre-classifier + curator, with isolation and fallback.
pub(crate) struct RouterClient {
    http: Client,
    chat_url: String,
    model: String,
    timeout: Duration,
    query_head_chars: usize,
    permits: Semaphore,
}

impl RouterClient {
    /// Builds a router client from configuration. Returns `None` when the router is
    /// disabled — the caller then uses the no-think default.
    #[must_use]
    pub(crate) fn from_config(cfg: &RouterConfig) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        let http = Client::builder()
            .connect_timeout(Duration::from_millis(cfg.timeout_ms))
            .build()
            .unwrap_or_default();
        Some(Self {
            http,
            chat_url: format!("{}/v1/chat/completions", cfg.endpoint.trim_end_matches('/')),
            model: cfg.model.clone(),
            timeout: Duration::from_millis(cfg.timeout_ms),
            query_head_chars: cfg.query_head_chars,
            permits: Semaphore::new(cfg.max_concurrent.max(1)),
        })
    }

    /// Resolves the reasoning axis for a query.
    ///
    /// Returns the decision, its observable [`ReasoningSource`], and the **SYSTEM** routing
    /// latency (metric 2 below). Never blocks the hot path: saturation / timeout / any
    /// error → no-think fallback.
    ///
    /// # Two distinct latency metrics (Bob 2026-07-10)
    ///
    /// 1. **Curator sub-path** (`router_curator_latency_ms`) — the LLM round-trip, logged
    ///    HERE and only when the boundary invokes the curator (~250 ms warm). It is NOT
    ///    under the SLA.
    /// 2. **SYSTEM routing decision** (returned `Duration`) — end-to-end over EVERY request,
    ///    including the pre-classifier fast path (near-zero when no curator call). The
    ///    `< 150 ms` SLA targets THIS metric; the pre-classifier keeps it low by cutting
    ///    curator volume.
    pub(crate) async fn resolve(
        &self,
        query: &str,
        image_present: bool,
        metrics: &crate::metrics::Metrics,
    ) -> (bool, ReasoningSource, Duration) {
        let start = Instant::now();
        let head: String = query.chars().take(self.query_head_chars).collect();

        // Tier 1: cheap deterministic pre-classifier (no curator call → SYSTEM latency stays low).
        // Tier 2: curator (boundary only), isolated + hard timeout. Metric 1 (curator sub-path)
        // is observed separately from the SYSTEM latency (metric 2), both fed to Prometheus.
        let (decision, source) = if let Some(decision) = preclassify(&head) {
            (decision, ReasoningSource::Router)
        } else {
            let curator_start = Instant::now();
            match self.call_curator(&head, image_present).await {
                Ok(think) => {
                    let elapsed = curator_start.elapsed();
                    metrics.observe_router_curator_latency(elapsed);
                    tracing::debug!(
                        router_curator_latency_ms = elapsed.as_millis() as u64,
                        "router: curator sub-path (metric 1, outside system SLA)"
                    );
                    (think, ReasoningSource::Router)
                }
                Err(e) => {
                    let elapsed = curator_start.elapsed();
                    metrics.observe_router_curator_latency(elapsed);
                    // Fallback métriqué au même point que le WARN (jamais silencieux — 01KWVXAWB3).
                    metrics.record_router_fallback(e.reason_label());
                    tracing::warn!(
                        reason = %e,
                        router_curator_latency_ms = elapsed.as_millis() as u64,
                        "router: no-think fallback (routing decision unavailable)"
                    );
                    (false, ReasoningSource::Fallback)
                }
            }
        };

        // Metric 2 (SYSTEM routing decision) — observée pour TOUTE requête (fast-path inclus).
        let system_latency = start.elapsed();
        metrics.observe_router_system_latency(system_latency);
        (decision, source, system_latency)
    }

    /// Calls the curator with the GBNF-constrained routing prompt.
    async fn call_curator(&self, query: &str, image_present: bool) -> Result<bool, RouterError> {
        // Isolation: fail fast on saturation rather than queueing on the hot path.
        let _permit = self
            .permits
            .try_acquire()
            .map_err(|_| RouterError::Saturated)?;

        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": ROUTER_SYSTEM_PROMPT },
                { "role": "user", "content": format!("QUERY: {query}\nIMAGE_PRESENT: {image_present}") },
            ],
            "temperature": 0,
            "max_tokens": 8,
            "grammar": ROUTER_GRAMMAR,
            "stream": false,
        });

        // R1: the hard timeout covers the ENTIRE round-trip — `send()` AND `resp.json()`.
        // A curator that accepts the request then stalls on the response body would
        // otherwise hold the isolation permit past `timeout` (starving the hot path); the
        // outer `timeout` bounds both phases and releases the permit on the fallback.
        let call = async {
            let resp = self
                .http
                .post(&self.chat_url)
                .json(&body)
                .send()
                .await
                .map_err(RouterError::Http)?;

            let status = resp.status();
            if !status.is_success() {
                return Err(RouterError::BadStatus(status.as_u16()));
            }

            let value: serde_json::Value = resp.json().await.map_err(RouterError::Http)?;
            value
                .pointer("/choices/0/message/content")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_label)
                .ok_or(RouterError::Unparseable)
        };

        tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| RouterError::Timeout)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouterConfig;
    use crate::metrics::Metrics;
    use std::collections::HashSet;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Metrics de test (allowlists vides — le routeur n'utilise pas les labels alias/route).
    fn test_metrics() -> Metrics {
        Metrics::new(0, HashSet::new(), HashSet::new(), HashSet::new())
    }

    fn router_cfg(endpoint: String) -> RouterConfig {
        RouterConfig {
            enabled: true,
            endpoint,
            model: "curator".to_string(),
            timeout_ms: 500,
            max_concurrent: 1,
            query_head_chars: 384,
        }
    }

    // --- Pre-classifier (d) ---

    #[test]
    fn preclassify_greeting_et_vide_no_think() {
        assert_eq!(preclassify(""), Some(false));
        assert_eq!(preclassify("  "), Some(false));
        assert_eq!(preclassify("hello there"), Some(false));
        assert_eq!(preclassify("MERCI"), Some(false));
    }

    #[test]
    fn preclassify_commande_systeme_courte_no_think() {
        assert_eq!(preclassify("restart the nginx service"), Some(false));
        assert_eq!(preclassify("stop gradatum-engine"), Some(false));
    }

    #[test]
    fn preclassify_trigger_raisonnement_va_a_la_frontiere() {
        // Un mot-déclencheur force la frontière (curator) — protège recall-THINK.
        assert_eq!(preclassify("why does the build fail"), None);
        assert_eq!(preclassify("restart and explain why it crashed"), None);
        assert_eq!(preclassify("design a migration plan"), None);
    }

    #[test]
    fn preclassify_lookup_factuel_va_a_la_frontiere() {
        // Un lookup factuel n'est PAS court-circuité (trop risqué) → curator tranche.
        assert_eq!(preclassify("what port does example-dns use"), None);
    }

    #[test]
    fn parse_label_strict() {
        assert_eq!(parse_label("THINK"), Some(true));
        assert_eq!(parse_label(" THINK"), Some(true));
        assert_eq!(parse_label("NO_THINK"), Some(false));
        assert_eq!(parse_label("maybe"), None);
    }

    #[test]
    fn from_config_disabled_none() {
        let cfg = RouterConfig::default(); // enabled = false
        assert!(RouterClient::from_config(&cfg).is_none());
    }

    // --- Curator call + fallback (a)(c) ---

    #[tokio::test]
    async fn resolve_preclassifie_ne_touche_pas_le_curator() {
        // (a) Cas évident → aucune requête réseau. Endpoint MORT : si le curator était
        // appelé, la source serait Fallback ; elle est Router → preclassify a court-circuité.
        let router = RouterClient::from_config(&router_cfg("http://127.0.0.1:1".to_string()))
            .expect("router activé");
        let (reasoning, source, latency) =
            router.resolve("hello there", false, &test_metrics()).await;
        assert!(!reasoning);
        assert_eq!(source, ReasoningSource::Router);
        // Métrique 2 (latence SYSTEM) : quasi nulle sur le fast-path pré-classifieur
        // (aucun appel curator) → soutient le SLA <150ms.
        assert!(
            latency < Duration::from_millis(10),
            "latence SYSTEM basse sans appel curator (métrique 2, SLA)"
        );
    }

    #[tokio::test]
    async fn resolve_frontiere_appelle_le_curator() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "THINK" } }]
            })))
            .mount(&server)
            .await;
        let router = RouterClient::from_config(&router_cfg(server.uri())).expect("router activé");
        // "what port does example-dns use" = frontière (pas de trigger, pas de commande).
        let (reasoning, source, _lat) = router
            .resolve("what port does example-dns use", false, &test_metrics())
            .await;
        assert!(reasoning, "curator a répondu THINK");
        assert_eq!(source, ReasoningSource::Router);
    }

    #[tokio::test]
    async fn resolve_curator_down_fallback_no_think() {
        // (c) Curator injoignable → fallback no-think (source Fallback, jamais silencieux).
        let router = RouterClient::from_config(&router_cfg("http://127.0.0.1:1".to_string()))
            .expect("router activé");
        let metrics = test_metrics();
        let (reasoning, source, _lat) = router
            .resolve("what port does example-dns use", false, &metrics)
            .await;
        assert!(!reasoning, "fallback = no-think");
        assert_eq!(source, ReasoningSource::Fallback);
        // Le fallback est métriqué (raison http : connexion refusée) — jamais silencieux.
        assert!(
            metrics
                .render()
                .contains("gateway_router_fallback_total{reason=\"http\"}"),
            "le fallback doit incrémenter gateway_router_fallback_total{{reason=http}}"
        );
    }

    #[tokio::test]
    async fn resolve_timeout_couvre_tout_lappel_fallback() {
        // R1 : le timeout borne l'ENSEMBLE de l'appel (send + lecture body). Un curateur
        // qui répond trop lentement → Timeout → fallback no-think (permit relâché).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(300))
                    .set_body_json(serde_json::json!({
                        "choices": [{ "message": { "content": "THINK" } }]
                    })),
            )
            .mount(&server)
            .await;
        let mut cfg = router_cfg(server.uri());
        cfg.timeout_ms = 80; // < 300 ms délai → timeout garanti
        let router = RouterClient::from_config(&cfg).expect("router activé");
        let metrics = test_metrics();
        let start = Instant::now();
        let (reasoning, source, _lat) = router
            .resolve("what port does example-dns use", false, &metrics)
            .await;
        assert!(!reasoning, "timeout → fallback no-think");
        assert_eq!(source, ReasoningSource::Fallback);
        assert!(
            metrics
                .render()
                .contains("gateway_router_fallback_total{reason=\"timeout\"}"),
            "un timeout doit incrémenter gateway_router_fallback_total{{reason=timeout}}"
        );
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "le fallback survient au timeout (~80ms), pas après la réponse complète (300ms)"
        );
    }

    #[tokio::test]
    async fn resolve_saturation_ne_famine_pas_fallback_rapide() {
        // (b) Saturation : permit unique épuisé → fallback immédiat (pas de blocage hot-path).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(400)) // maintient le permit occupé
                    .set_body_json(serde_json::json!({
                        "choices": [{ "message": { "content": "THINK" } }]
                    })),
            )
            .mount(&server)
            .await;
        let router = std::sync::Arc::new(
            RouterClient::from_config(&router_cfg(server.uri())).expect("router activé"),
        );
        let metrics = test_metrics();
        // Occupe l'unique permit avec un appel frontière en vol.
        let busy = {
            let r = router.clone();
            let m = metrics.clone();
            tokio::spawn(
                async move { r.resolve("what port does example-dns use", false, &m).await },
            )
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Second appel frontière concurrent → saturé → fallback rapide.
        let start = Instant::now();
        let (reasoning, source, _lat) = router
            .resolve("what config value is set", false, &metrics)
            .await;
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "fallback immédiat sur saturation, pas de famine"
        );
        assert!(!reasoning);
        assert_eq!(source, ReasoningSource::Fallback);
        assert!(
            metrics
                .render()
                .contains("gateway_router_fallback_total{reason=\"saturated\"}"),
            "la saturation doit incrémenter gateway_router_fallback_total{{reason=saturated}}"
        );
        let _ = busy.await;
    }
}
