//! VaultAware hook — fire-and-forget `QaEvent` delivery to the gradatum event log.
//!
//! Sends quality events (latency, model used, route) to the internal API
//! via asynchronous batched POST requests.
//!
//! Guarantees:
//! - Never blocking: the send on the mpsc channel is non-blocking (`try_send`).
//!   If the channel is full, the event is silently dropped.
//! - Never propagates errors: the hook is best-effort. A failing endpoint does not
//!   degrade the gateway service.
//! - No-op when `event_log_endpoint` is absent.
//!
//! Architecture: a background task drains the mpsc channel and flushes by batch
//! (N=10 events or T=5 s, whichever comes first).

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::commons::chat::Usage;
use crate::config::VaultAwareConfig;

/// Cost-attribution data for a `QaEvent`.
///
/// Groups optional fields to stay within the clippy `too_many_arguments`
/// limit (≤7) in `make_qa_event`.
#[derive(Debug, Default)]
pub struct CostAttribution<'a> {
    /// Feature ID extracted from the `X-Feature-Id` header.
    pub feature_id: Option<String>,
    /// Real model resolved by the gateway (≠ client alias).
    pub model_used: Option<String>,
    /// Token statistics from the provider (`None` for streaming or slot-passthrough).
    pub usage: Option<&'a Usage>,
    /// Emitting agent identifier — extracted from the `X-Agent-Id` header.
    ///
    /// `None` when the header is absent.
    pub agent_id: Option<String>,
}

/// Quality event sent to the event log.
///
/// ## Wire alignment with `QaEventDto` (gradatum-dto)
///
/// JSON field names are strictly aligned with the server-side `QaEventDto`.
/// The 5 optional fields use `#[serde(skip_serializing_if)]` to avoid serializing
/// explicit `null` (the server DTO declares `#[serde(default)]` to accept absent fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaEvent {
    /// HTTP route (e.g. `"/v1/chat/completions"`).
    pub route: String,
    /// Model alias used by the client.
    pub model_alias: String,
    /// Effective provider resolved (primary or fallback).
    pub provider: String,
    /// HTTP status code returned to the client.
    pub status_code: u16,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// ISO 8601 timestamp of the request.
    pub timestamp: String,
    // ── Cost-attribution fields (QaEventDto) ──
    /// Feature ID extracted from the `X-Feature-Id` header — absent when the header is not provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<String>,
    /// Real model resolved by the gateway (≠ alias) — key for future pricing.
    ///
    /// `None` when not resolved (slot-passthrough path or upstream error before resolution).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,
    /// Prompt tokens (`usage.prompt_tokens`) — `None` for streaming requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_input: Option<u32>,
    /// Completion tokens (`usage.completion_tokens`) — `None` for streaming requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_output: Option<u32>,
    /// USD cost — always `None` (no pricing table implemented yet).
    ///
    /// Forward-compat: field present in the DTO and the server SQLite table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f32>,
    /// Emitting agent identifier — extracted from the `X-Agent-Id` header.
    ///
    /// `None` when the header is absent — backward-compatible (server declares `#[serde(default)]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// Sender to the VaultAware background task.
///
/// `None` when the hook is disabled (no endpoint configured).
#[derive(Clone)]
pub struct VaultAwareSender(Option<mpsc::Sender<QaEvent>>);

impl VaultAwareSender {
    /// Builds a disabled (no-op) sender.
    pub fn disabled() -> Self {
        VaultAwareSender(None)
    }

    /// Sends an event to the background task without blocking.
    ///
    /// Silently dropped when the channel is full or the hook is disabled.
    pub fn send_event(&self, event: QaEvent) {
        if let Some(tx) = &self.0 {
            // try_send: non-blocking. Drop on full channel (silent backpressure).
            if let Err(e) = tx.try_send(event) {
                tracing::debug!("vault_aware event dropped (channel full or closed): {}", e);
            }
        }
    }

    /// Returns `true` if the hook is active.
    pub fn is_active(&self) -> bool {
        self.0.is_some()
    }
}

/// Starts the VaultAware background task and returns the sender.
///
/// Returns `VaultAwareSender::disabled()` when `config.event_log_endpoint` is absent.
/// Returns `Err(reqwest::Error)` if the HTTP client fails to build (TLS unavailable).
///
/// The background task flushes events to the HTTP endpoint in batches:
/// - Immediate flush when `batch_size` events have accumulated.
/// - Periodic flush every `flush_interval_secs` seconds.
pub fn start_vault_aware_task(
    config: Arc<VaultAwareConfig>,
) -> Result<VaultAwareSender, reqwest::Error> {
    let endpoint = match &config.event_log_endpoint {
        Some(ep) if !ep.is_empty() => {
            if !ep.starts_with("http://") && !ep.starts_with("https://") {
                tracing::warn!(
                    endpoint = %ep,
                    "vault_aware: invalid endpoint (must start with http:// or https://) — hook disabled"
                );
                return Ok(VaultAwareSender::disabled());
            }
            ep.clone()
        }
        _ => {
            tracing::debug!("vault_aware disabled — no endpoint configured");
            return Ok(VaultAwareSender::disabled());
        }
    };

    let batch_size = config.batch_size;
    let flush_interval = Duration::from_secs(config.flush_interval_secs);

    // Buffered channel with backpressure: capacity = 10× batch size.
    let (tx, mut rx) = mpsc::channel::<QaEvent>(batch_size * 10);

    // Clone endpoint_url BEFORE capturing it in the spawn (the `endpoint` variable is
    // also used afterwards in the tracing::info! call below).
    let endpoint_url = endpoint.clone();

    // INVARIANT: reqwest::Client::builder().build() can only fail when the system
    // has no native TLS — detectable at service startup, not a silent runtime error.
    // Propagate the error so the caller decides the fallback.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    tokio::spawn(async move {
        let mut batch: Vec<QaEvent> = Vec::with_capacity(batch_size);
        let mut interval = tokio::time::interval(flush_interval);

        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(ev) => {
                            batch.push(ev);
                            if batch.len() >= batch_size {
                                flush_batch(&client, &endpoint_url, &mut batch).await;
                            }
                        }
                        None => {
                            // Channel closed (shutdown) — flush remaining events and exit.
                            if !batch.is_empty() {
                                flush_batch(&client, &endpoint_url, &mut batch).await;
                            }
                            break;
                        }
                    }
                }
                _ = interval.tick() => {
                    if !batch.is_empty() {
                        flush_batch(&client, &endpoint_url, &mut batch).await;
                    }
                }
            }
        }

        tracing::debug!("vault_aware background task finished");
    });

    tracing::info!(
        endpoint = %endpoint,
        batch_size = batch_size,
        flush_interval_secs = flush_interval.as_secs(),
        "vault_aware hook enabled"
    );

    Ok(VaultAwareSender(Some(tx)))
}

/// Flushes the current batch to the endpoint.
///
/// HTTP errors are silently ignored — the hook is best-effort.
async fn flush_batch(client: &reqwest::Client, endpoint: &str, batch: &mut Vec<QaEvent>) {
    let events: Vec<QaEvent> = std::mem::take(batch);
    let count = events.len();

    match client.post(endpoint).json(&events).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!(count = count, "vault_aware batch flushed");
        }
        Ok(resp) => {
            tracing::debug!(
                status = resp.status().as_u16(),
                count = count,
                "vault_aware batch flush: non-200 response (ignored)"
            );
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                count = count,
                "vault_aware batch flush: network error (ignored)"
            );
        }
    }
}

/// Builds a `QaEvent` from the metadata of a processed request.
///
/// Cost-attribution fields are grouped in [`CostAttribution`] to stay within
/// the clippy `too_many_arguments` limit (≤7).
///
/// - `attr.usage`: `None` for streaming (SSE chunks without an aggregate) or the
///   slot-passthrough path. When `Some`, `tokens_input` ← `usage.prompt_tokens`,
///   `tokens_output` ← `usage.completion_tokens`.
pub fn make_qa_event(
    route: &str,
    model_alias: &str,
    provider: &str,
    status_code: u16,
    latency_ms: u64,
    attr: CostAttribution<'_>,
) -> QaEvent {
    let (tokens_input, tokens_output) = match attr.usage {
        Some(u) => (Some(u.prompt_tokens), Some(u.completion_tokens)),
        None => (None, None),
    };

    QaEvent {
        route: route.to_owned(),
        model_alias: model_alias.to_owned(),
        provider: provider.to_owned(),
        status_code,
        latency_ms,
        timestamp: chrono::Utc::now().to_rfc3339(),
        feature_id: attr.feature_id,
        model_used: attr.model_used,
        tokens_input,
        tokens_output,
        cost_usd: None, // toujours None en v0.3.0 — pas de table de pricing
        agent_id: attr.agent_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commons::chat::Usage;

    /// Helper : construit un `Usage` de test.
    fn test_usage(prompt: u32, completion: u32) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            prompt_tokens_details: None,
        }
    }

    #[test]
    fn test_disabled_sender_send_is_noop() {
        let sender = VaultAwareSender::disabled();
        assert!(!sender.is_active());
        // Ne doit pas paniquer.
        sender.send_event(make_qa_event(
            "/v1/chat/completions",
            "alias",
            "provider",
            200,
            42,
            CostAttribution::default(),
        ));
    }

    #[test]
    fn test_make_qa_event_fields_basiques() {
        let ev = make_qa_event(
            "/v1/embeddings",
            "embed-alias",
            "test-provider",
            200,
            17,
            CostAttribution::default(),
        );
        assert_eq!(ev.route, "/v1/embeddings");
        assert_eq!(ev.model_alias, "embed-alias");
        assert_eq!(ev.provider, "test-provider");
        assert_eq!(ev.status_code, 200);
        assert_eq!(ev.latency_ms, 17);
        assert!(!ev.timestamp.is_empty());
        // Champs optionnels absents.
        assert!(ev.feature_id.is_none());
        assert!(ev.model_used.is_none());
        assert!(ev.tokens_input.is_none());
        assert!(ev.tokens_output.is_none());
        assert!(ev.cost_usd.is_none());
    }

    /// `make_qa_event` peuple feature_id, model_used, tokens depuis Usage.
    #[test]
    fn test_make_qa_event_avec_usage() {
        let usage = test_usage(42, 17);
        let ev = make_qa_event(
            "/v1/chat/completions",
            "gpt4o",
            "openai",
            200,
            350,
            CostAttribution {
                feature_id: Some("feat-search".to_string()),
                model_used: Some("gpt-4o-2024-05-13".to_string()),
                usage: Some(&usage),
                agent_id: None,
            },
        );
        assert_eq!(ev.feature_id.as_deref(), Some("feat-search"));
        assert_eq!(ev.model_used.as_deref(), Some("gpt-4o-2024-05-13"));
        assert_eq!(ev.tokens_input, Some(42));
        assert_eq!(ev.tokens_output, Some(17));
        assert!(ev.cost_usd.is_none(), "cost_usd toujours None en v0.3.0");
    }

    /// Requête streamée → tokens None même si feature_id et model_used sont présents.
    #[test]
    fn test_make_qa_event_streaming_tokens_none() {
        let ev = make_qa_event(
            "/v1/chat/completions",
            "qwen",
            "test-provider",
            200,
            1200,
            CostAttribution {
                feature_id: Some("feat-agent".to_string()),
                model_used: Some("qwen3-32b".to_string()),
                usage: None, // streaming : pas d'usage
                agent_id: None,
            },
        );
        assert_eq!(ev.feature_id.as_deref(), Some("feat-agent"));
        assert_eq!(ev.model_used.as_deref(), Some("qwen3-32b"));
        assert!(ev.tokens_input.is_none(), "streaming → tokens_input None");
        assert!(ev.tokens_output.is_none(), "streaming → tokens_output None");
    }

    /// Round-trip serde : `QaEvent` sérialisé en JSON puis désérialisé.
    ///
    /// Vérifie que le fil JSON est compatible entre gateway et serveur.
    #[test]
    fn test_qa_event_serde_round_trip_avec_tous_champs() {
        let usage = test_usage(100, 50);
        let ev = make_qa_event(
            "/v1/chat/completions",
            "alias-test",
            "provider-test",
            200,
            123,
            CostAttribution {
                feature_id: Some("feature-xyz".to_string()),
                model_used: Some("model-real-v2".to_string()),
                usage: Some(&usage),
                agent_id: None,
            },
        );

        // Sérialiser le QaEvent gateway.
        let json_str = serde_json::to_string(&ev).expect("sérialisation QaEvent");

        // Désérialiser en QaEvent (round-trip symétrique).
        let ev2: QaEvent =
            serde_json::from_str(&json_str).expect("désérialisation QaEvent round-trip");
        assert_eq!(ev2.route, ev.route);
        assert_eq!(ev2.model_alias, ev.model_alias);
        assert_eq!(ev2.provider, ev.provider);
        assert_eq!(ev2.status_code, ev.status_code);
        assert_eq!(ev2.latency_ms, ev.latency_ms);
        assert_eq!(ev2.feature_id, ev.feature_id);
        assert_eq!(ev2.model_used, ev.model_used);
        assert_eq!(ev2.tokens_input, ev.tokens_input);
        assert_eq!(ev2.tokens_output, ev.tokens_output);
        assert_eq!(ev2.cost_usd, ev.cost_usd);
    }

    /// Round-trip serde avec champs None : le JSON ne doit PAS contenir les clés
    /// pour les champs `skip_serializing_if = "Option::is_none"`.
    #[test]
    fn test_qa_event_serde_none_fields_omis() {
        let ev = make_qa_event(
            "/route",
            "alias",
            "prov",
            200,
            10,
            CostAttribution::default(),
        );
        let json_str = serde_json::to_string(&ev).expect("sérialisation");
        let json_val: serde_json::Value = serde_json::from_str(&json_str).expect("désérialisation");
        // Les clés optionnelles ne doivent pas apparaître si None.
        assert!(
            !json_val.as_object().unwrap().contains_key("feature_id"),
            "feature_id absent si None"
        );
        assert!(
            !json_val.as_object().unwrap().contains_key("model_used"),
            "model_used absent si None"
        );
        assert!(
            !json_val.as_object().unwrap().contains_key("tokens_input"),
            "tokens_input absent si None"
        );
        assert!(
            !json_val.as_object().unwrap().contains_key("tokens_output"),
            "tokens_output absent si None"
        );
        assert!(
            !json_val.as_object().unwrap().contains_key("cost_usd"),
            "cost_usd absent si None"
        );
    }

    /// R2 : provider ne retourne pas d'usage (completion.usage = None) →
    /// QaEvent.tokens_input et tokens_output sont None (pas de panic, propagation propre).
    ///
    /// Couvre le chemin non-streaming où le backend omet le champ `usage`
    /// (comportement observé sur certains providers compatibles OpenAI).
    #[test]
    fn test_make_qa_event_usage_none_no_panic() {
        // Construction explicite avec usage = None (pas streaming — juste usage absent).
        let ev = make_qa_event(
            "/v1/chat/completions",
            "alias-sans-usage",
            "provider-avare",
            200,
            250,
            CostAttribution {
                feature_id: Some("feat-no-usage".to_string()),
                model_used: Some("model-real-v1".to_string()),
                usage: None,
                agent_id: None,
            },
        );
        // Pas de panic — propagation propre.
        assert_eq!(
            ev.tokens_input, None,
            "usage=None → tokens_input doit être None"
        );
        assert_eq!(
            ev.tokens_output, None,
            "usage=None → tokens_output doit être None"
        );
        // Les autres champs restent correctement peuplés.
        assert_eq!(ev.model_used.as_deref(), Some("model-real-v1"));
        assert_eq!(ev.feature_id.as_deref(), Some("feat-no-usage"));
        assert_eq!(ev.status_code, 200);
        assert!(ev.cost_usd.is_none(), "cost_usd toujours None en v0.3.0");
    }

    #[tokio::test]
    async fn test_active_sender_is_active() {
        let config = Arc::new(VaultAwareConfig {
            event_log_endpoint: Some("http://127.0.0.1:19099/api/v1/event-log".to_string()),
            batch_size: 10,
            flush_interval_secs: 5,
        });
        let sender = start_vault_aware_task(config)
            .expect("construction client HTTP doit réussir dans l'environnement de test");
        assert!(sender.is_active());
        // Envoi ne doit pas paniquer même si l'endpoint est KO.
        sender.send_event(make_qa_event(
            "/v1/chat/completions",
            "a",
            "p",
            200,
            5,
            CostAttribution::default(),
        ));
    }

    // ── Tests agent_id ────────────────────────────────────────────────────────

    /// make_qa_event peuple agent_id depuis CostAttribution.
    #[test]
    fn test_make_qa_event_avec_agent_id() {
        let ev = make_qa_event(
            "/v1/chat/completions",
            "alias",
            "provider",
            200,
            100,
            CostAttribution {
                agent_id: Some("example-agent".to_string()),
                ..CostAttribution::default()
            },
        );
        assert_eq!(
            ev.agent_id.as_deref(),
            Some("example-agent"),
            "agent_id doit être peuplé depuis CostAttribution"
        );
    }

    /// make_qa_event sans agent_id → field None (CostAttribution::default()).
    #[test]
    fn test_make_qa_event_sans_agent_id_none() {
        let ev = make_qa_event(
            "/v1/chat/completions",
            "alias",
            "provider",
            200,
            100,
            CostAttribution::default(),
        );
        assert!(ev.agent_id.is_none(), "agent_id absent → None");
    }

    /// Alignement de fil JSON `agent_id` gateway ↔ serveur.
    ///
    /// QaEvent gateway sérialise `agent_id` sous la même clé JSON que QaEventDto serveur.
    /// Vérifié en inspectant la valeur brute du JSON (sans import cross-crate).
    /// Le test symétrique cross-crate (QaEvent→QaEventDto) vit dans les tests d'intégration.
    #[test]
    fn test_qa_event_agent_id_json_key_aligne() {
        let ev = make_qa_event(
            "/v1/chat/completions",
            "alias-rt",
            "provider-rt",
            200,
            77,
            CostAttribution {
                feature_id: Some("feat-rt".to_string()),
                agent_id: Some("agent-42".to_string()),
                ..CostAttribution::default()
            },
        );

        let json_str = serde_json::to_string(&ev).expect("sérialisation QaEvent");
        let json_val: serde_json::Value =
            serde_json::from_str(&json_str).expect("désérialisation QaEvent");

        // Vérifier que la clé JSON est bien "agent_id" (pas "agentId" ou autre).
        assert_eq!(
            json_val["agent_id"].as_str(),
            Some("agent-42"),
            "clé JSON doit être 'agent_id' (alignement fil gateway↔serveur)"
        );
        assert_eq!(
            json_val["feature_id"].as_str(),
            Some("feat-rt"),
            "feature_id doit être présent"
        );
    }

    /// Round-trip avec agent_id None : `agent_id` absent du JSON (skip_serializing_if).
    #[test]
    fn test_qa_event_agent_id_none_omis_du_json() {
        let ev = make_qa_event(
            "/route",
            "alias",
            "prov",
            200,
            10,
            CostAttribution::default(),
        );
        let json_str = serde_json::to_string(&ev).expect("sérialisation");
        let json_val: serde_json::Value = serde_json::from_str(&json_str).expect("désérialisation");
        assert!(
            !json_val.as_object().unwrap().contains_key("agent_id"),
            "agent_id None → absent du JSON (skip_serializing_if)"
        );
    }

    /// Endpoint avec schéma invalide (ftp://) → hook désactivé (fail-safe, FIX 3 SSRF guard).
    ///
    /// Un endpoint non-HTTP ne doit pas déclencher de requête sortante — le hook
    /// est désactivé silencieusement avec un `warn!` (Option A fail-safe).
    #[test]
    fn invalid_endpoint_schema_disables_hook() {
        let config = Arc::new(VaultAwareConfig {
            event_log_endpoint: Some("ftp://malicious.internal/foo".to_string()),
            batch_size: 10,
            flush_interval_secs: 5,
        });
        let sender = start_vault_aware_task(config)
            .expect("construction ne doit pas échouer (schéma invalide → désactivé)");
        assert!(
            !sender.is_active(),
            "endpoint schéma invalide (ftp://) → sender doit être inactif"
        );
    }

    /// Borne 256 chars : QaEvent produit par make_qa_event avec agent_id de 256 chars passe.
    ///
    /// La borne 256 est appliquée dans chat.rs avant make_qa_event.
    /// Ce test vérifie que make_qa_event accepte 256 chars sans troncature.
    #[test]
    fn test_make_qa_event_agent_id_256_chars_passe() {
        let long_agent_id = "a".repeat(256);
        let ev = make_qa_event(
            "/v1/chat/completions",
            "alias",
            "provider",
            200,
            10,
            CostAttribution {
                agent_id: Some(long_agent_id.clone()),
                ..CostAttribution::default()
            },
        );
        assert_eq!(
            ev.agent_id.as_deref(),
            Some(long_agent_id.as_str()),
            "agent_id 256 chars doit être conservé tel quel"
        );
    }
}
