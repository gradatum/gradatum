//! Prometheus metrics export — native implementation with no external dependency.
//!
//! Exposed metrics:
//! - `gateway_requests_total{route,model_alias,provider,status_code}` — counter
//! - `gateway_request_duration_seconds_sum/count{route,model_alias}` — summary
//! - `gateway_providers_configured` — gauge
//! - `gateway_uptime_seconds` — gauge
//!
//! ## Bounded cardinality — `route`, `model_alias`, `provider` labels
//!
//! All three labels are filtered through an allowlist:
//!
//! - `model_alias`: allowlist of aliases configured in `[aliases]`. An unknown alias
//!   is replaced by `"unknown"`.
//! - `route`: finite set of routes declared in the Axum router.
//!   An unknown route (arbitrary injected path) is replaced by `"other"`.
//! - `provider`: set of provider names configured at startup.
//!   An unknown provider (e.g. unconfigured dynamic fallback) is replaced by `"other"`.
//!
//! This bounds the total cardinality to
//!   `(|routes| + 1) × (|aliases| + 1) × (|providers| + 1) × |status_codes|`
//! and prevents unbounded memory growth (DoS via label injection).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestLabels {
    pub route: String,
    pub model_alias: String,
    pub provider: String,
    pub status_code: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DurationLabels {
    pub route: String,
    pub model_alias: String,
}

/// Shared metrics registry.
#[derive(Clone)]
pub struct Metrics {
    inner: Arc<MetricsInner>,
}

struct MetricsInner {
    requests_total: Mutex<HashMap<RequestLabels, u64>>,
    duration_ms_sum: Mutex<HashMap<DurationLabels, u64>>,
    duration_count: Mutex<HashMap<DurationLabels, u64>>,
    providers_configured: u64,
    start_time: Instant,
    /// Allowlist of configured aliases — bounds the cardinality of the `model_alias` label.
    /// Aliases absent from this set are counted under `"unknown"`.
    known_aliases: HashSet<String>,
    /// Allowlist of routes declared in the Axum router — bounds the cardinality of `route`.
    /// Arbitrary paths (injection) are counted under `"other"`.
    known_routes: HashSet<String>,
    /// Allowlist of providers configured at startup — bounds the cardinality of `provider`.
    /// Unknown providers (e.g. dynamic fallbacks) are counted under `"other"`.
    known_providers: HashSet<String>,
}

impl Metrics {
    /// Builds the metrics registry.
    ///
    /// - `known_aliases`: set of aliases configured in `[aliases]`. Only these aliases
    ///   are accepted as the `model_alias` label. Unknown aliases are replaced by
    ///   `"unknown"` to bound cardinality.
    /// - `known_routes`: finite set of Axum routes (e.g. `"/v1/chat/completions"`).
    ///   Arbitrary paths are replaced by `"other"`.
    /// - `known_providers`: names of providers configured at startup. Unconfigured
    ///   providers are replaced by `"other"`.
    pub fn new(
        providers_configured: usize,
        known_aliases: HashSet<String>,
        known_routes: HashSet<String>,
        known_providers: HashSet<String>,
    ) -> Self {
        Self {
            inner: Arc::new(MetricsInner {
                requests_total: Mutex::new(HashMap::new()),
                duration_ms_sum: Mutex::new(HashMap::new()),
                duration_count: Mutex::new(HashMap::new()),
                providers_configured: providers_configured as u64,
                start_time: Instant::now(),
                known_aliases,
                known_routes,
                known_providers,
            }),
        }
    }

    /// Sanitizes `model_alias`: returns the alias if known, `"unknown"` otherwise.
    ///
    /// Bounds the label cardinality to `len(configured_aliases) + 1`.
    fn sanitize_alias<'a>(&self, model_alias: &'a str) -> std::borrow::Cow<'a, str> {
        if self.inner.known_aliases.contains(model_alias) {
            std::borrow::Cow::Borrowed(model_alias)
        } else {
            std::borrow::Cow::Owned("unknown".to_owned())
        }
    }

    /// Sanitizes `route`: returns the route if known, `"other"` otherwise.
    ///
    /// Bounds the `route` label cardinality to `len(known_routes) + 1`.
    fn sanitize_route<'a>(&self, route: &'a str) -> std::borrow::Cow<'a, str> {
        if self.inner.known_routes.contains(route) {
            std::borrow::Cow::Borrowed(route)
        } else {
            std::borrow::Cow::Owned("other".to_owned())
        }
    }

    /// Sanitizes `provider`: returns the provider if known, `"other"` otherwise.
    ///
    /// Bounds the `provider` label cardinality to `len(known_providers) + 1`.
    fn sanitize_provider<'a>(&self, provider: &'a str) -> std::borrow::Cow<'a, str> {
        if self.inner.known_providers.contains(provider) {
            std::borrow::Cow::Borrowed(provider)
        } else {
            std::borrow::Cow::Owned("other".to_owned())
        }
    }

    /// Increments `gateway_requests_total` and records the latency when provided.
    ///
    /// The `route`, `model_alias`, and `provider` labels are sanitized through their
    /// respective allowlists to prevent unbounded memory growth via label injection.
    /// Values outside the allowlist → `"other"` (route/provider) or
    /// `"unknown"` (model_alias, to preserve dashboard backward compatibility).
    pub fn record_request(
        &self,
        route: &str,
        model_alias: &str,
        provider: &str,
        status_code: u16,
        latency: Option<Duration>,
    ) {
        let safe_route = self.sanitize_route(route);
        let safe_alias = self.sanitize_alias(model_alias);
        let safe_provider = self.sanitize_provider(provider);

        let req_labels = RequestLabels {
            route: safe_route.as_ref().to_owned(),
            model_alias: safe_alias.as_ref().to_owned(),
            provider: safe_provider.as_ref().to_owned(),
            status_code,
        };

        if let Ok(mut map) = self.inner.requests_total.lock() {
            *map.entry(req_labels).or_insert(0) += 1;
        }

        if let Some(dur) = latency {
            let dur_labels = DurationLabels {
                route: safe_route.into_owned(),
                model_alias: safe_alias.into_owned(),
            };
            let ms = dur.as_millis() as u64;

            if let Ok(mut map) = self.inner.duration_ms_sum.lock() {
                *map.entry(dur_labels.clone()).or_insert(0) += ms;
            }
            if let Ok(mut map) = self.inner.duration_count.lock() {
                *map.entry(dur_labels).or_insert(0) += 1;
            }
        }
    }

    /// Renders the Prometheus text format 0.0.4 export.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(2048);

        out.push_str(
            "# HELP gateway_requests_total Nombre total de requetes traitees par le gateway.\n",
        );
        out.push_str("# TYPE gateway_requests_total counter\n");
        if let Ok(map) = self.inner.requests_total.lock() {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(k, _)| (&k.route, &k.model_alias, &k.provider, k.status_code));
            for (labels, count) in entries {
                out.push_str(&format!(
                    "gateway_requests_total{{route=\"{}\",model_alias=\"{}\",provider=\"{}\",status_code=\"{}\"}} {}\n",
                    escape_label(&labels.route),
                    escape_label(&labels.model_alias),
                    escape_label(&labels.provider),
                    labels.status_code,
                    count,
                ));
            }
        }

        out.push_str("# HELP gateway_request_duration_seconds Duree des requetes en secondes.\n");
        out.push_str("# TYPE gateway_request_duration_seconds summary\n");
        if let (Ok(sum_map), Ok(count_map)) = (
            self.inner.duration_ms_sum.lock(),
            self.inner.duration_count.lock(),
        ) {
            let mut keys: Vec<_> = sum_map.keys().collect();
            keys.sort_by_key(|k| (&k.route, &k.model_alias));
            for key in keys {
                let sum_ms = sum_map.get(key).copied().unwrap_or(0);
                let count = count_map.get(key).copied().unwrap_or(0);
                let sum_secs = sum_ms as f64 / 1000.0;
                out.push_str(&format!(
                    "gateway_request_duration_seconds_sum{{route=\"{}\",model_alias=\"{}\"}} {:.6}\n",
                    escape_label(&key.route),
                    escape_label(&key.model_alias),
                    sum_secs,
                ));
                out.push_str(&format!(
                    "gateway_request_duration_seconds_count{{route=\"{}\",model_alias=\"{}\"}} {}\n",
                    escape_label(&key.route),
                    escape_label(&key.model_alias),
                    count,
                ));
            }
        }

        out.push_str(
            "# HELP gateway_providers_configured Nombre de providers configures au demarrage.\n",
        );
        out.push_str("# TYPE gateway_providers_configured gauge\n");
        out.push_str(&format!(
            "gateway_providers_configured {}\n",
            self.inner.providers_configured
        ));

        let uptime = self.inner.start_time.elapsed().as_secs_f64();
        out.push_str("# HELP gateway_uptime_seconds Duree de vie du gateway en secondes.\n");
        out.push_str("# TYPE gateway_uptime_seconds gauge\n");
        out.push_str(&format!("gateway_uptime_seconds {:.3}\n", uptime));

        out
    }
}

fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(vals: &[&str]) -> HashSet<String> {
        vals.iter().map(|s| s.to_string()).collect()
    }

    /// Construit un Metrics de test avec des allowlists standard.
    ///
    /// - aliases : liste fournie
    /// - routes : routes Axum réelles du gateway
    /// - providers : liste fournie
    fn mk_metrics(aliases: &[&str], providers: &[&str]) -> Metrics {
        Metrics::new(
            providers.len(),
            known(aliases),
            known(&[
                "/v1/chat/completions",
                "/v1/embeddings",
                "/v1/rerank",
                "/v1/models",
                "/health",
                "/metrics",
            ]),
            known(providers),
        )
    }

    #[test]
    fn test_metrics_render_requests_total_present() {
        let m = mk_metrics(&["my-alias"], &["test-provider"]);
        m.record_request(
            "/v1/chat/completions",
            "my-alias",
            "test-provider",
            200,
            Some(Duration::from_millis(50)),
        );
        let output = m.render();
        assert!(output.contains("# TYPE gateway_requests_total counter"));
        assert!(output.contains("gateway_requests_total{"));
    }

    #[test]
    fn test_metrics_render_providers_configured() {
        let m = Metrics::new(3, known(&[]), known(&[]), known(&[]));
        let output = m.render();
        assert!(output.contains("gateway_providers_configured 3"));
    }

    #[test]
    fn test_escape_label_quotes() {
        assert_eq!(escape_label("hello\"world"), "hello\\\"world");
        assert_eq!(escape_label("back\\slash"), "back\\\\slash");
    }

    /// Sécurité P2 : un alias inconnu (input user non filtré) est compté sous
    /// `"unknown"` et non inséré tel quel dans la HashMap.
    /// Vérifie la borne de cardinalité : N aliases + 1 entrée "unknown" max.
    #[test]
    fn test_unknown_alias_counted_under_unknown_label() {
        let m = mk_metrics(&["embed", "curator"], &["p1", "p2"]);

        // Alias connu → label préservé
        m.record_request("/v1/chat/completions", "embed", "p1", 200, None);
        // Alias inconnu → compté sous "unknown"
        m.record_request(
            "/v1/chat/completions",
            "injected-evil-alias-AAAA",
            "p1",
            200,
            None,
        );
        m.record_request(
            "/v1/chat/completions",
            "another-unknown-BBBB",
            "p1",
            200,
            None,
        );
        // Alias connu → compteur incrémenté, pas de nouvelle clé
        m.record_request("/v1/chat/completions", "curator", "p2", 200, None);

        let output = m.render();

        // Les alias inconnus ne doivent pas apparaître dans le rendu
        assert!(
            !output.contains("injected-evil-alias-AAAA"),
            "alias inconnu ne doit pas apparaître dans le rendu Prometheus"
        );
        assert!(
            !output.contains("another-unknown-BBBB"),
            "alias inconnu ne doit pas apparaître dans le rendu Prometheus"
        );

        // "unknown" doit apparaître (les deux inconnus sont fusionnés)
        assert!(
            output.contains("unknown"),
            "les aliases inconnus doivent être comptés sous 'unknown'"
        );

        // Cardinalité bornée : map requests_total ≤ nb_aliases_configurés + 1 entrées distinctes par route
        // (embed × 1 + curator × 1 + unknown × 1 = 3 entrées pour /v1/chat/completions, 3 ≤ 2+1)
        assert!(
            output.contains("model_alias=\"embed\""),
            "alias configuré 'embed' doit apparaître"
        );
        assert!(
            output.contains("model_alias=\"curator\""),
            "alias configuré 'curator' doit apparaître"
        );
    }

    /// Vérifie que plusieurs appels avec des aliases inconnus différents produisent
    /// UNE SEULE entrée "unknown" (pas N entrées → cardinalité bornée).
    #[test]
    fn test_unknown_aliases_merged_single_entry() {
        let m = mk_metrics(&["valid-alias"], &["prov"]);

        // 100 aliases inconnus distincts → doivent tous aller dans "unknown"
        for i in 0..100 {
            m.record_request(
                "/v1/embeddings",
                &format!("unknown-alias-{}", i),
                "prov",
                200,
                None,
            );
        }

        let map = m.inner.requests_total.lock().expect("lock requests_total");
        // Seule l'entrée "unknown" doit exister (1 clé, pas 100)
        assert_eq!(
            map.len(),
            1,
            "100 aliases inconnus distincts ne doivent produire qu'1 entrée 'unknown' (cardinalité bornée)"
        );
        let (labels, count) = map.iter().next().expect("au moins 1 entrée");
        assert_eq!(labels.model_alias, "unknown");
        assert_eq!(*count, 100, "le compteur 'unknown' doit valoir 100");
    }

    /// Vérifie que `route` et `provider` inconnus tombent dans `"other"`.
    ///
    /// ## Invariant Fix B
    ///
    /// Un path arbitraire (injection) ou un provider inconnu ne doivent pas créer
    /// de nouvelles clés dans la HashMap → cardinalité bornée.
    #[test]
    fn b_unknown_route_and_provider_fall_into_other() {
        let m = mk_metrics(&["alias-a"], &["provider-known"]);

        // Route connue + provider connu → labels préservés.
        m.record_request(
            "/v1/chat/completions",
            "alias-a",
            "provider-known",
            200,
            None,
        );

        // Route inconnue → "other".
        m.record_request(
            "/admin/../../etc/passwd",
            "alias-a",
            "provider-known",
            404,
            None,
        );
        m.record_request("/unknown-path-ZZZZ", "alias-a", "provider-known", 404, None);

        // Provider inconnu → "other".
        m.record_request(
            "/v1/chat/completions",
            "alias-a",
            "injected-provider-XXXX",
            200,
            None,
        );
        m.record_request(
            "/v1/chat/completions",
            "alias-a",
            "injected-provider-YYYY",
            200,
            None,
        );

        let output = m.render();

        // Les paths inconnus ne doivent pas apparaître.
        assert!(
            !output.contains("/admin/../../etc/passwd"),
            "b: route inconnue ne doit pas apparaître dans le rendu"
        );
        assert!(
            !output.contains("unknown-path-ZZZZ"),
            "b: route inconnue ne doit pas apparaître dans le rendu"
        );
        // Les providers inconnus ne doivent pas apparaître.
        assert!(
            !output.contains("injected-provider-XXXX"),
            "b: provider inconnu ne doit pas apparaître dans le rendu"
        );
        assert!(
            !output.contains("injected-provider-YYYY"),
            "b: provider inconnu ne doit pas apparaître dans le rendu"
        );

        // "other" doit apparaître pour les routes et providers inconnus.
        let map = m.inner.requests_total.lock().expect("lock");
        let routes_in_map: std::collections::HashSet<&str> =
            map.keys().map(|k| k.route.as_str()).collect();
        assert!(
            routes_in_map.contains("other"),
            "b: routes inconnues doivent être comptées sous 'other', routes présentes : {:?}",
            routes_in_map
        );
        let providers_in_map: std::collections::HashSet<&str> =
            map.keys().map(|k| k.provider.as_str()).collect();
        assert!(
            providers_in_map.contains("other"),
            "b: providers inconnus doivent être comptés sous 'other', providers présents : {:?}",
            providers_in_map
        );

        // Cardinalité bornée : les 5 appels ne produisent que 3 clés distinctes
        // (/v1/chat/completions×alias-a×provider-known + other×alias-a×provider-known
        //  + /v1/chat/completions×alias-a×other)
        assert!(
            map.len() <= 3,
            "b: cardinalité doit être bornée (≤3 clés pour ces 5 appels, map.len={})",
            map.len()
        );
    }

    /// Vérifie qu'un provider connu conserve son label dans les métriques.
    #[test]
    fn b_known_provider_label_preserved() {
        let m = mk_metrics(&["alias-x"], &["backend-primary", "backend-fallback"]);

        m.record_request(
            "/v1/chat/completions",
            "alias-x",
            "backend-primary",
            200,
            None,
        );
        m.record_request("/v1/embeddings", "alias-x", "backend-fallback", 200, None);

        let output = m.render();
        assert!(
            output.contains("provider=\"backend-primary\""),
            "b: provider connu doit apparaître dans le rendu"
        );
        assert!(
            output.contains("provider=\"backend-fallback\""),
            "b: provider connu doit apparaître dans le rendu"
        );
        // Aucune valeur "other" pour ces appels.
        assert!(
            !output.contains("provider=\"other\""),
            "b: pas de 'other' pour des providers connus"
        );
    }
}
