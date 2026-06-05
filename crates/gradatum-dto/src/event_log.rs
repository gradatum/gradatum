use serde::{Deserialize, Serialize};

/// Événement de qualité reçu depuis le gateway — contrat wire JSON.
///
/// Miroir de `gradatum_gateway::vault_aware::QaEvent` côté serveur.
/// Les champs JSON sont strictement alignés pour une désérialisation sans perte.
///
/// ## Alignement de fil
///
/// | Champ JSON         | Source gateway              | Requis ? |
/// |--------------------|----------------------------|----------|
/// | `route`            | QaEvent.route               | Oui      |
/// | `model_alias`      | QaEvent.model_alias         | Oui      |
/// | `provider`         | QaEvent.provider            | Oui      |
/// | `status_code`      | QaEvent.status_code (u16)   | Oui      |
/// | `latency_ms`       | QaEvent.latency_ms (u64)    | Oui      |
/// | `timestamp`        | QaEvent.timestamp (RFC3339) | Oui      |
/// | `feature_id`       | header X-Feature-Id         | Non      |
/// | `model_used`       | modèle réel résolu          | Non      |
/// | `tokens_input`     | usage.prompt_tokens         | Non      |
/// | `tokens_output`    | usage.completion_tokens     | Non      |
/// | `cost_usd`         | toujours None v0.3.0        | Non      |
/// | `agent_id`         | header X-Agent-Id           | Non      |
///
/// Les champs optionnels utilisent `#[serde(default)]` pour désérialiser
/// `null` ou l'absence de champ en `None` — les POSTs actuels du gateway
/// (6 champs) sont ainsi acceptés sans erreur.
///
/// `tenant_id` n'est PAS dans ce DTO — il est extrait du JWT via `TrustContext`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaEventDto {
    /// Route HTTP concernée (ex: `/v1/chat/completions`).
    pub route: String,
    /// Alias modèle utilisé par le client.
    pub model_alias: String,
    /// Provider effectif résolu (primary ou fallback).
    pub provider: String,
    /// Code HTTP retourné au client.
    pub status_code: u16,
    /// Latence end-to-end en millisecondes.
    pub latency_ms: u64,
    /// Timestamp ISO8601 / RFC3339 de la requête (ex: `2026-06-01T12:00:00Z`).
    pub timestamp: String,
    /// ID feature extrait du header `X-Feature-Id` — absent si header non fourni.
    #[serde(default)]
    pub feature_id: Option<String>,
    /// Modèle réel résolu par le gateway (≠ alias) — clé du pricing futur.
    #[serde(default)]
    pub model_used: Option<String>,
    /// Tokens prompt (None si requête streamée ou gateway pre-B1).
    #[serde(default)]
    pub tokens_input: Option<u32>,
    /// Tokens completion (None si requête streamée ou gateway pre-B1).
    #[serde(default)]
    pub tokens_output: Option<u32>,
    /// Coût USD — toujours `None` en v0.3.0 (pas de table de pricing).
    ///
    /// Forward-compat : le champ est présent dans le DTO et la table SQLite
    /// pour que les POSTs futurs puissent le valoriser sans migration de schéma.
    #[serde(default)]
    pub cost_usd: Option<f32>,
    /// Identifiant de l'agent émetteur — extrait du header `X-Agent-Id`.
    ///
    /// Discriminateur pour l'apprentissage transverse vs propre-au-rôle.
    /// ACL/filtrage par agent_id différé à v0.4.0 — on capture la donnée maintenant.
    ///
    /// `None` si le header est absent (rétrocompat : les callers actuels sans agent_id
    /// désérialisent avec `None` grâce à `#[serde(default)]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// Réponse de l'endpoint `POST /api/v1/event-log`.
///
/// Retournée avec `200 OK` si l'ingestion a réussi.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogResponse {
    /// Nombre d'événements effectivement insérés.
    pub accepted_count: usize,
    /// Statut textuel — toujours `"accepted"` en succès.
    pub status: String,
}
