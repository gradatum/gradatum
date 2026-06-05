//! Export métriques Prometheus — implémentation native sans dépendance externe.
//!
//! Métriques exposées :
//! - `gateway_requests_total{route,model_alias,provider,status_code}` — counter
//! - `gateway_request_duration_seconds_sum/count{route,model_alias}` — summary
//! - `gateway_providers_configured` — gauge
//! - `gateway_uptime_seconds` — gauge

use std::collections::HashMap;
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

/// Registre de métriques partagé.
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
}

impl Metrics {
    pub fn new(providers_configured: usize) -> Self {
        Self {
            inner: Arc::new(MetricsInner {
                requests_total: Mutex::new(HashMap::new()),
                duration_ms_sum: Mutex::new(HashMap::new()),
                duration_count: Mutex::new(HashMap::new()),
                providers_configured: providers_configured as u64,
                start_time: Instant::now(),
            }),
        }
    }

    /// Incrémente `gateway_requests_total` et enregistre la latence si fournie.
    pub fn record_request(
        &self,
        route: &str,
        model_alias: &str,
        provider: &str,
        status_code: u16,
        latency: Option<Duration>,
    ) {
        let req_labels = RequestLabels {
            route: route.to_owned(),
            model_alias: model_alias.to_owned(),
            provider: provider.to_owned(),
            status_code,
        };

        if let Ok(mut map) = self.inner.requests_total.lock() {
            *map.entry(req_labels).or_insert(0) += 1;
        }

        if let Some(dur) = latency {
            let dur_labels = DurationLabels {
                route: route.to_owned(),
                model_alias: model_alias.to_owned(),
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

    /// Produit l'export Prometheus en text format 0.0.4.
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

    #[test]
    fn test_metrics_render_requests_total_present() {
        let m = Metrics::new(2);
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
        let m = Metrics::new(3);
        let output = m.render();
        assert!(output.contains("gateway_providers_configured 3"));
    }

    #[test]
    fn test_escape_label_quotes() {
        assert_eq!(escape_label("hello\"world"), "hello\\\"world");
        assert_eq!(escape_label("back\\slash"), "back\\\\slash");
    }
}
