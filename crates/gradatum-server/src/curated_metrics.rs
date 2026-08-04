//! Curated Prometheus metric selection for studio timeseries (since v0.7.5).
//!
//! `prometheus_client` is write+encode only (no per-series getters). The registry is
//! re-encoded as OpenMetrics text (same path as `/metrics`) and then parsed through a
//! **closed allowlist**. Histograms are reduced to `_sum` + `_count` (the per-interval
//! average is computed on the frontend). Decoupled from instrumentation.

use std::collections::HashMap;

use prometheus_client::encoding::text::encode;

use crate::metrics::AppMetrics;

/// Métadonnée d'une série curée (exposée par le catalog `/system/metrics/catalog`).
#[derive(Debug, Clone)]
pub struct CuratedSeriesMeta {
    /// Clé de série curée (ex. `"mcp_tool_calls.vault_write"`, ou un préfixe stub `"curator.decisions"`).
    pub key: String,
    /// Groupe : `"usage"` | `"context"` | `"server"` | `"write"`.
    pub group: &'static str,
    /// Type : `"counter"` | `"gauge"` | `"histogram_sum"` | `"histogram_count"`.
    pub kind: &'static str,
    /// Unité indicative (`"calls"`, `"seconds"`, `"rows"`, `"tokens"`, …).
    pub unit: &'static str,
    /// `false` pour les familles stub non encore alimentées (curator/llm).
    pub instrumented: bool,
}

/// Une règle d'allowlist : comment mapper une ligne OpenMetrics vers une clé curée.
struct Rule {
    /// Nom de métrique EXACT tel qu'émis par `encode` (suffixe inclus : `_total`/`_sum`/`_count`).
    emitted: &'static str,
    /// Label à plier dans la clé (`Some("endpoint")`), ou `None` si agrégé / sans label.
    fold_label: Option<&'static str>,
    /// Préfixe de la clé curée résultante.
    prefix: &'static str,
    /// `true` → toutes les lignes de cette métrique sont SOMMÉES dans une clé unique (agrégation).
    aggregate: bool,
    group: &'static str,
    kind: &'static str,
    unit: &'static str,
}

/// Allowlist statique. Les noms `emitted` DOIVENT correspondre à la sortie réelle de `encode`
/// (verified against `encode` output). Counters use the `_total` suffix; histograms use `_sum`/`_count`.
const RULES: &[Rule] = &[
    // ── Usage read-path ──
    Rule {
        emitted: "gradatum_read_usage_total",
        fold_label: Some("endpoint"),
        prefix: "read_usage",
        aggregate: false,
        group: "usage",
        kind: "counter",
        unit: "calls",
    },
    Rule {
        emitted: "gradatum_mcp_tool_calls_total",
        fold_label: Some("tool"),
        prefix: "mcp_tool_calls",
        aggregate: false,
        group: "usage",
        kind: "counter",
        unit: "calls",
    },
    // ── Efficacité contexte ──
    Rule {
        emitted: "gradatum_vault_context_duration_seconds_sum",
        fold_label: Some("mode"),
        prefix: "vault_context.duration_sum",
        aggregate: false,
        group: "context",
        kind: "histogram_sum",
        unit: "seconds",
    },
    Rule {
        emitted: "gradatum_vault_context_duration_seconds_count",
        fold_label: Some("mode"),
        prefix: "vault_context.duration_count",
        aggregate: false,
        group: "context",
        kind: "histogram_count",
        unit: "count",
    },
    Rule {
        emitted: "gradatum_vault_context_embed_fallback_total",
        fold_label: Some("mode"),
        prefix: "vault_context.embed_fallback",
        aggregate: false,
        group: "context",
        kind: "counter",
        unit: "count",
    },
    Rule {
        emitted: "gradatum_vault_context_candidates_sum",
        fold_label: Some("mode"),
        prefix: "vault_context.candidates_sum",
        aggregate: false,
        group: "context",
        kind: "histogram_sum",
        unit: "count",
    },
    Rule {
        emitted: "gradatum_vault_context_candidates_count",
        fold_label: Some("mode"),
        prefix: "vault_context.candidates_count",
        aggregate: false,
        group: "context",
        kind: "histogram_count",
        unit: "count",
    },
    Rule {
        emitted: "gradatum_vault_context_included_sum",
        fold_label: Some("mode"),
        prefix: "vault_context.included_sum",
        aggregate: false,
        group: "context",
        kind: "histogram_sum",
        unit: "count",
    },
    Rule {
        emitted: "gradatum_vault_context_included_count",
        fold_label: Some("mode"),
        prefix: "vault_context.included_count",
        aggregate: false,
        group: "context",
        kind: "histogram_count",
        unit: "count",
    },
    Rule {
        emitted: "gradatum_context_inline_total",
        fold_label: Some("mode"),
        prefix: "context.inline",
        aggregate: false,
        group: "context",
        kind: "counter",
        unit: "notes",
    },
    Rule {
        emitted: "gradatum_context_stub_total",
        fold_label: Some("mode"),
        prefix: "context.stub",
        aggregate: false,
        group: "context",
        kind: "counter",
        unit: "stubs",
    },
    Rule {
        emitted: "gradatum_context_dropped_total",
        fold_label: Some("mode"),
        prefix: "context.dropped",
        aggregate: false,
        group: "context",
        kind: "counter",
        unit: "notes",
    },
    Rule {
        emitted: "gradatum_context_compaction_total",
        fold_label: None,
        prefix: "context.compaction_total",
        aggregate: false,
        group: "context",
        kind: "counter",
        unit: "calls",
    },
    Rule {
        emitted: "gradatum_context_tokens_saved_sum",
        fold_label: None,
        prefix: "context.tokens_saved_sum",
        aggregate: false,
        group: "context",
        kind: "histogram_sum",
        unit: "tokens",
    },
    Rule {
        emitted: "gradatum_context_tokens_saved_count",
        fold_label: None,
        prefix: "context.tokens_saved_count",
        aggregate: false,
        group: "context",
        kind: "histogram_count",
        unit: "count",
    },
    Rule {
        emitted: "gradatum_vault_proactive_recall_surfaced_total",
        fold_label: Some("mode"),
        prefix: "proactive.surfaced",
        aggregate: false,
        group: "context",
        kind: "counter",
        unit: "hits",
    },
    Rule {
        emitted: "gradatum_vault_proactive_recall_accepted_total",
        fold_label: None,
        prefix: "proactive.accepted",
        aggregate: false,
        group: "context",
        kind: "counter",
        unit: "hits",
    },
    Rule {
        emitted: "gradatum_vault_proactive_refresh_total",
        fold_label: None,
        prefix: "proactive.refresh",
        aggregate: false,
        group: "context",
        kind: "counter",
        unit: "refreshes",
    },
    Rule {
        emitted: "gradatum_vault_proactive_recall_duration_seconds_sum",
        fold_label: Some("mode"),
        prefix: "proactive.recall_duration_sum",
        aggregate: false,
        group: "context",
        kind: "histogram_sum",
        unit: "seconds",
    },
    Rule {
        emitted: "gradatum_vault_proactive_recall_duration_seconds_count",
        fold_label: Some("mode"),
        prefix: "proactive.recall_duration_count",
        aggregate: false,
        group: "context",
        kind: "histogram_count",
        unit: "count",
    },
    Rule {
        emitted: "gradatum_vault_proactive_refresh_duration_seconds_sum",
        fold_label: None,
        prefix: "proactive.refresh_duration_sum",
        aggregate: false,
        group: "context",
        kind: "histogram_sum",
        unit: "seconds",
    },
    Rule {
        emitted: "gradatum_vault_proactive_refresh_duration_seconds_count",
        fold_label: None,
        prefix: "proactive.refresh_duration_count",
        aggregate: false,
        group: "context",
        kind: "histogram_count",
        unit: "count",
    },
    // ── Santé serveur ──
    Rule {
        emitted: "gradatum_http_requests_total",
        fold_label: None,
        prefix: "http.requests_total",
        aggregate: true,
        group: "server",
        kind: "counter",
        unit: "requests",
    },
    Rule {
        emitted: "gradatum_http_request_duration_seconds_sum",
        fold_label: None,
        prefix: "http.duration_sum",
        aggregate: true,
        group: "server",
        kind: "histogram_sum",
        unit: "seconds",
    },
    Rule {
        emitted: "gradatum_http_request_duration_seconds_count",
        fold_label: None,
        prefix: "http.duration_count",
        aggregate: true,
        group: "server",
        kind: "histogram_count",
        unit: "count",
    },
    Rule {
        emitted: "gradatum_queue_depth",
        fold_label: Some("tenant"),
        prefix: "queue.depth",
        aggregate: false,
        group: "server",
        kind: "gauge",
        unit: "items",
    },
    Rule {
        emitted: "gradatum_queue_lag_seconds",
        fold_label: Some("tenant"),
        prefix: "queue.lag",
        aggregate: false,
        group: "server",
        kind: "gauge",
        unit: "seconds",
    },
    Rule {
        emitted: "gradatum_auth_failures_total",
        fold_label: Some("reason"),
        prefix: "auth.failures",
        aggregate: false,
        group: "server",
        kind: "counter",
        unit: "failures",
    },
    Rule {
        emitted: "gradatum_event_log_rows",
        fold_label: None,
        prefix: "event_log.rows",
        aggregate: false,
        group: "server",
        kind: "gauge",
        unit: "rows",
    },
    Rule {
        emitted: "gradatum_review_promoted_total",
        fold_label: None,
        prefix: "review.promoted_total",
        aggregate: true,
        group: "server",
        kind: "counter",
        unit: "notes",
    },
    Rule {
        emitted: "gradatum_review_promote_errors_total",
        fold_label: None,
        prefix: "review.promote_errors",
        aggregate: false,
        group: "server",
        kind: "counter",
        unit: "errors",
    },
    // ── Pipeline d'écriture ──
    Rule {
        emitted: "gradatum_write_check_total",
        fold_label: Some("rule"),
        prefix: "write_check",
        aggregate: false,
        group: "write",
        kind: "counter",
        unit: "violations",
    },
    Rule {
        emitted: "gradatum_curator_decisions_total",
        // F-66 : fold over `path` → per-path totals (`curator.decisions.fast_admit`, …),
        // the traffic-share-by-decision-path signal the tuning gate consumes.
        fold_label: Some("path"),
        prefix: "curator.decisions",
        aggregate: false,
        group: "write",
        kind: "counter",
        unit: "decisions",
    },
    Rule {
        emitted: "gradatum_llm_backend_calls_total",
        fold_label: Some("backend"),
        prefix: "llm.calls",
        aggregate: false,
        group: "write",
        kind: "counter",
        unit: "calls",
    },
];

/// Special-case : pour `http.requests_total`, on émet AUSSI `http.errors_5xx_total`
/// (somme des lignes dont le label `status` commence par `5`).
const HTTP_REQUESTS_EMITTED: &str = "gradatum_http_requests_total";

/// Préfixes de familles stub toujours listés au catalog (à 0, non instrumentés).
const STUB_PREFIXES: &[(&str, &str, &str)] = &[
    // (prefix, group, unit)
    // curator.decisions is now instrumented (F-66) — only llm.calls remains a stub.
    ("llm.calls", "write", "calls"),
];

/// Collecte la photo curée courante du registry. Best-effort : si l'encode échoue,
/// retourne `Vec::new()` (jamais de panic — l'appelant est une tâche background).
pub fn collect_curated_samples(metrics: &AppMetrics) -> Vec<(String, f64)> {
    let mut buf = String::new();
    if encode(&mut buf, &metrics.registry).is_err() {
        return Vec::new();
    }
    // Accumulateur : sum-into. Les clés agrégées reçoivent plusieurs contributions ;
    // les clés à label distinct reçoivent une seule valeur (somme == valeur).
    let mut acc: HashMap<String, f64> = HashMap::new();

    for line in buf.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, labels, value)) = parse_line(line) else {
            continue;
        };
        // Ignorer les lignes _created et _bucket (non listées dans RULES de toute façon,
        // mais garde explicite pour le futur).
        if name.ends_with("_created") || name.ends_with("_bucket") {
            continue;
        }

        for rule in RULES {
            if rule.emitted != name {
                continue;
            }
            // Cas spécial http 5xx (en plus de l'agrégat total).
            if name == HTTP_REQUESTS_EMITTED
                && let Some(status) = labels.get("status")
                && status.starts_with('5')
            {
                *acc.entry("http.errors_5xx_total".to_string())
                    .or_insert(0.0) += value;
            }
            let key = if rule.aggregate {
                rule.prefix.to_string()
            } else if let Some(lbl) = rule.fold_label {
                match labels.get(lbl) {
                    Some(v) => format!("{}.{}", rule.prefix, v),
                    None => continue, // label attendu absent → ligne ignorée
                }
            } else {
                // fold_label est None, aggregate est false : clé sans label
                rule.prefix.to_string()
            };
            *acc.entry(key).or_insert(0.0) += value;
            break;
        }
    }

    let mut out: Vec<(String, f64)> = acc.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Parse une ligne OpenMetrics `name{l="v",...} 12.5` ou `name 12.5`.
/// Retourne `(name_sans_labels, labels_map, value)`.
fn parse_line(line: &str) -> Option<(&str, HashMap<String, String>, f64)> {
    // Valeur = dernier token après le dernier espace.
    let sp = line.rfind(' ')?;
    let (left, val_str) = (line[..sp].trim(), line[sp + 1..].trim());
    let value: f64 = val_str.parse().ok()?;

    let mut labels = HashMap::new();
    let name = if let Some(brace) = left.find('{') {
        let name = &left[..brace];
        let inner = &left[brace + 1..left.rfind('}')?];
        for pair in split_top_level_commas(inner) {
            if let Some(eq) = pair.find('=') {
                let k = pair[..eq].trim().to_string();
                let v = pair[eq + 1..].trim().trim_matches('"').to_string();
                labels.insert(k, v);
            }
        }
        name
    } else {
        left
    };
    Some((name, labels, value))
}

/// Découpe `a="x",b="y"` en pairs en respectant les guillemets (les valeurs peuvent contenir des virgules).
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quotes = false;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(s[start..].trim());
    }
    out
}

/// Résout la métadonnée d'une clé curée (par préfixe le plus long). `None` si hors allowlist.
pub fn series_meta(series_key: &str) -> Option<CuratedSeriesMeta> {
    // http.errors_5xx_total : dérivé, même groupe que http.
    if series_key == "http.errors_5xx_total" {
        return Some(CuratedSeriesMeta {
            key: series_key.to_string(),
            group: "server",
            kind: "counter",
            unit: "errors",
            instrumented: true,
        });
    }
    let mut best: Option<&Rule> = None;
    for rule in RULES {
        // Match exact (prefix sans label) OU prefix suivi d'un '.'.
        let hit = series_key == rule.prefix || series_key.starts_with(&format!("{}.", rule.prefix));
        if hit && best.is_none_or(|b| rule.prefix.len() > b.prefix.len()) {
            best = Some(rule);
        }
    }
    best.map(|rule| CuratedSeriesMeta {
        key: series_key.to_string(),
        group: rule.group,
        kind: rule.kind,
        unit: rule.unit,
        instrumented: !matches!(rule.prefix, "llm.calls"),
    })
}

/// Entrées stub toujours visibles au catalog (familles non encore alimentées).
pub fn stub_catalog_entries() -> Vec<CuratedSeriesMeta> {
    STUB_PREFIXES
        .iter()
        .map(|(prefix, group, unit)| CuratedSeriesMeta {
            key: prefix.to_string(),
            group,
            kind: "counter",
            unit,
            instrumented: false,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests TDD (Task 2 Step 1)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{AppMetrics, ContextAssemblyLabel, HttpReqLabels, UsageEndpointLabel};

    // Helper : récupère la valeur d'une clé curée dans le résultat de collect.
    fn val(samples: &[(String, f64)], key: &str) -> Option<f64> {
        samples.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    #[test]
    fn collect_maps_counter_family_by_label() {
        let m = AppMetrics::new();
        // read_usage{endpoint="search"} += 3
        m.read_usage
            .get_or_create(&UsageEndpointLabel {
                endpoint: "search".into(),
            })
            .inc_by(3);
        let s = collect_curated_samples(&m);
        assert_eq!(val(&s, "read_usage.search"), Some(3.0));
    }

    #[test]
    fn collect_extracts_histogram_sum_and_count() {
        let m = AppMetrics::new();
        // vault_context_duration_seconds{mode="assembled"} observe 0.5
        m.vault_context_duration
            .get_or_create(&ContextAssemblyLabel { mode: "assembled" })
            .observe(0.5);
        let s = collect_curated_samples(&m);
        assert_eq!(val(&s, "vault_context.duration_count.assembled"), Some(1.0));
        assert!((val(&s, "vault_context.duration_sum.assembled").unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn collect_aggregates_http_requests_across_paths() {
        let m = AppMetrics::new();
        // Deux séries http distinctes (path/status) → agrégées en http.requests_total.
        m.http_requests
            .get_or_create(&HttpReqLabels {
                method: "GET".into(),
                path: "/api/v1/vault_search".into(),
                status: 200,
            })
            .inc();
        m.http_requests
            .get_or_create(&HttpReqLabels {
                method: "GET".into(),
                path: "/api/v1/vault_read".into(),
                status: 503,
            })
            .inc();
        let s = collect_curated_samples(&m);
        assert_eq!(
            val(&s, "http.requests_total"),
            Some(2.0),
            "somme toutes séries"
        );
        assert_eq!(val(&s, "http.errors_5xx_total"), Some(1.0), "1 statut 5xx");
    }

    #[test]
    fn collect_ignores_non_allowlisted_series() {
        let m = AppMetrics::new();
        // revocation_store_size n'est pas dans l'allowlist.
        m.revocation_size.set(42_i64);
        let s = collect_curated_samples(&m);
        assert!(s.iter().all(|(k, _)| !k.contains("revocation")));
    }

    #[test]
    fn series_meta_resolves_group_by_prefix() {
        assert_eq!(
            series_meta("mcp_tool_calls.vault_write").unwrap().group,
            "usage"
        );
        assert_eq!(series_meta("http.requests_total").unwrap().group, "server");
        assert_eq!(
            series_meta("write_check.entity_drift").unwrap().group,
            "write"
        );
        assert!(series_meta("unknown.thing").is_none());
    }

    #[test]
    fn stub_catalog_entries_are_marked_not_instrumented() {
        let stubs = stub_catalog_entries();
        // curator.decisions is instrumented since F-66 → no longer a stub entry.
        assert!(stubs.iter().all(|e| e.key != "curator.decisions"));
        assert!(
            stubs
                .iter()
                .any(|e| e.key == "llm.calls" && !e.instrumented)
        );
    }
}
