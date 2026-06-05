//! Hook VaultAware v81 — QaEvent fire-and-forget vers l'event-log gradatum.
//!
//! Le hook envoie des événements de qualité (latence, modèle utilisé, route)
//! à l'API interne via POST en batch asynchrone.
//!
//! Garanties :
//! - JAMAIS bloquant : le send sur le canal mpsc est non-bloquant (try_send).
//!   Si le canal est plein, l'événement est droppé silencieusement.
//! - JAMAIS d'erreur propagée : le hook est best-effort. Un endpoint KO ne dégrade
//!   pas le service gateway.
//! - No-op si `event_log_endpoint` est absent.
//!
//! Architecture : un background task consomme le canal mpsc et flush par batch
//! (N=10 événements ou T=5s selon le premier critère atteint).

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::commons::chat::Usage;
use crate::config::VaultAwareConfig;

/// Données d'attribution coût pour un `QaEvent`.
///
/// Regroupe les champs optionnels ajoutés en B1 pour contourner la limite
/// clippy `too_many_arguments` (>7) dans `make_qa_event`.
#[derive(Debug, Default)]
pub struct CostAttribution<'a> {
    /// ID feature extrait du header `X-Feature-Id`.
    pub feature_id: Option<String>,
    /// Modèle réel résolu par le gateway (≠ alias client).
    pub model_used: Option<String>,
    /// Statistiques tokens du provider (`None` si streaming ou slot-passthrough).
    pub usage: Option<&'a Usage>,
    /// Identifiant de l'agent émetteur — extrait du header `X-Agent-Id`.
    ///
    /// Discriminateur pour l'apprentissage transverse vs propre-au-rôle.
    /// `None` si le header est absent.
    pub agent_id: Option<String>,
}

/// Événement de qualité envoyé au log d'événements.
///
/// ## Alignement de fil avec `QaEventDto` (gradatum-dto)
///
/// Les noms de champs JSON sont strictement alignés sur `QaEventDto` côté serveur.
/// Les 5 champs optionnels utilisent `#[serde(skip_serializing_if)]` pour ne pas
/// sérialiser `null` explicitement (le DTO serveur déclare `#[serde(default)]` pour
/// accepter l'absence du champ).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaEvent {
    /// Route HTTP concernée (ex: "/v1/chat/completions").
    pub route: String,
    /// Alias modèle utilisé par le client.
    pub model_alias: String,
    /// Provider effectif résolu (primary ou fallback).
    pub provider: String,
    /// Code HTTP retourné au client.
    pub status_code: u16,
    /// Latence end-to-end en millisecondes.
    pub latency_ms: u64,
    /// Timestamp ISO8601 de la requête.
    pub timestamp: String,
    // ── Champs cost-attribution (v81 §cost-attribution / QaEventDto aligné) ──
    /// ID feature extrait du header `X-Feature-Id` — absent si header non fourni.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<String>,
    /// Modèle réel résolu par le gateway (≠ alias) — clé du pricing futur.
    ///
    /// `None` si non résolu (chemin slot passthrough ou erreur amont avant résolution).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,
    /// Tokens prompt (`usage.prompt_tokens`) — `None` si requête streamée.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_input: Option<u32>,
    /// Tokens completion (`usage.completion_tokens`) — `None` si requête streamée.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_output: Option<u32>,
    /// Coût USD — toujours `None` en v0.3.0 (pas de table de pricing).
    ///
    /// Forward-compat : présent dans le DTO et la table SQLite serveur.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f32>,
    /// Identifiant de l'agent émetteur — extrait du header `X-Agent-Id`.
    ///
    /// Discriminateur pour l'apprentissage transverse vs propre-au-rôle.
    /// `None` si le header est absent — rétrocompat (serveur déclare `#[serde(default)]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// Sender vers le background task VaultAware.
///
/// `None` si le hook est désactivé (endpoint non configuré).
#[derive(Clone)]
pub struct VaultAwareSender(Option<mpsc::Sender<QaEvent>>);

impl VaultAwareSender {
    /// Construit un sender désactivé (no-op).
    pub fn disabled() -> Self {
        VaultAwareSender(None)
    }

    /// Envoie un événement au background task, sans bloquer.
    ///
    /// Drop silencieux si le canal est plein ou si le hook est désactivé.
    pub fn send_event(&self, event: QaEvent) {
        if let Some(tx) = &self.0 {
            // try_send : non-bloquant. Drop si canal plein (backpressure silencieuse).
            if let Err(e) = tx.try_send(event) {
                tracing::debug!("vault_aware event droppé (canal plein ou fermé): {}", e);
            }
        }
    }

    /// Retourne `true` si le hook est actif.
    pub fn is_active(&self) -> bool {
        self.0.is_some()
    }
}

/// Démarre le background task VaultAware et retourne le sender.
///
/// Si `config.event_log_endpoint` est absent → retourne `VaultAwareSender::disabled()`.
/// Si la construction du client HTTP échoue (TLS manquant) → retourne `Err(reqwest::Error)`.
///
/// Le background task flush les événements vers l'endpoint HTTP en batch :
/// - Flush immédiat quand `batch_size` événements accumulés.
/// - Flush périodique toutes les `flush_interval_secs` secondes.
pub fn start_vault_aware_task(
    config: Arc<VaultAwareConfig>,
) -> Result<VaultAwareSender, reqwest::Error> {
    let endpoint = match &config.event_log_endpoint {
        Some(ep) if !ep.is_empty() => ep.clone(),
        _ => {
            tracing::debug!("vault_aware désactivé — aucun endpoint configuré");
            return Ok(VaultAwareSender::disabled());
        }
    };

    let batch_size = config.batch_size;
    let flush_interval = Duration::from_secs(config.flush_interval_secs);

    // Canal avec backpressure : capacité = 10× la taille de batch.
    let (tx, mut rx) = mpsc::channel::<QaEvent>(batch_size * 10);

    // Cloner endpoint_url AVANT de le capturer dans le spawn (la variable `endpoint` est
    // également utilisée après dans le tracing::info! ci-dessous).
    let endpoint_url = endpoint.clone();

    // INVARIANT : reqwest::Client::builder().build() ne peut échouer que si le système
    // n'expose pas de TLS natif — condition détectable au démarrage du service, pas un cas
    // d'erreur runtime silencieux. On propage l'erreur pour que l'appelant décide du fallback.
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
                            // Canal fermé (shutdown) — flush le reste et terminer.
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

        tracing::debug!("vault_aware background task terminé");
    });

    tracing::info!(
        endpoint = %endpoint,
        batch_size = batch_size,
        flush_interval_secs = flush_interval.as_secs(),
        "vault_aware hook activé"
    );

    Ok(VaultAwareSender(Some(tx)))
}

/// Flush le batch courant vers l'endpoint.
///
/// Erreurs HTTP ignorées silencieusement — le hook est best-effort.
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
                "vault_aware batch flush: réponse non-200 (ignorée)"
            );
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                count = count,
                "vault_aware batch flush: erreur réseau (ignorée)"
            );
        }
    }
}

/// Construit un `QaEvent` depuis les métadonnées d'une requête traitée.
///
/// Les champs cost-attribution sont groupés dans [`CostAttribution`] pour
/// respecter la limite clippy `too_many_arguments` (≤7).
///
/// - `attr.usage` : `None` pour le streaming (chunks SSE sans agrégat) ou le
///   chemin slot-passthrough. Quand `Some`, `tokens_input` ← `usage.prompt_tokens`,
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
                agent_id: Some("claude-backend-agent".to_string()),
                ..CostAttribution::default()
            },
        );
        assert_eq!(
            ev.agent_id.as_deref(),
            Some("claude-backend-agent"),
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
