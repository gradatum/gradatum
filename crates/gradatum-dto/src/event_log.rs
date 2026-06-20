use serde::{Deserialize, Serialize};

/// Quality event received from the gateway — JSON wire contract.
///
/// Mirror of `gradatum_gateway::vault_aware::QaEvent` on the server side.
/// JSON fields are strictly aligned for lossless deserialization.
///
/// ## Wire alignment
///
/// | JSON field         | Gateway source              | Required? |
/// |--------------------|----------------------------|-----------|
/// | `route`            | QaEvent.route               | Yes       |
/// | `model_alias`      | QaEvent.model_alias         | Yes       |
/// | `provider`         | QaEvent.provider            | Yes       |
/// | `status_code`      | QaEvent.status_code (u16)   | Yes       |
/// | `latency_ms`       | QaEvent.latency_ms (u64)    | Yes       |
/// | `timestamp`        | QaEvent.timestamp (RFC3339) | Yes       |
/// | `feature_id`       | header X-Feature-Id         | No        |
/// | `model_used`       | resolved real model         | No        |
/// | `tokens_input`     | usage.prompt_tokens         | No        |
/// | `tokens_output`    | usage.completion_tokens     | No        |
/// | `cost_usd`         | always None (no pricing table) | No     |
/// | `agent_id`         | header X-Agent-Id           | No        |
///
/// Optional fields use `#[serde(default)]` to deserialize `null` or an absent field
/// as `None` — gateway POSTs with only the required fields are accepted without error.
///
/// `tenant_id` is NOT in this DTO — it is extracted from the JWT via `TrustContext`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaEventDto {
    /// HTTP route (e.g. `/v1/chat/completions`).
    pub route: String,
    /// Model alias used by the client.
    pub model_alias: String,
    /// Effective provider resolved (primary or fallback).
    pub provider: String,
    /// HTTP status code returned to the client.
    pub status_code: u16,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// ISO8601 / RFC3339 timestamp of the request (e.g. `2026-06-01T12:00:00Z`).
    pub timestamp: String,
    /// Feature ID extracted from the `X-Feature-Id` header — absent if the header was not provided.
    #[serde(default)]
    pub feature_id: Option<String>,
    /// Real model resolved by the gateway (≠ alias).
    #[serde(default)]
    pub model_used: Option<String>,
    /// Prompt tokens (`None` if the request was streamed or gateway did not report usage).
    #[serde(default)]
    pub tokens_input: Option<u32>,
    /// Completion tokens (`None` if the request was streamed or gateway did not report usage).
    #[serde(default)]
    pub tokens_output: Option<u32>,
    /// Cost in USD — always `None` when no pricing table is available.
    ///
    /// Forward-compatible: the field is present in the DTO and in the SQLite table so
    /// that future POSTs can populate it without a schema migration.
    #[serde(default)]
    pub cost_usd: Option<f32>,
    /// Identifier of the emitting agent — extracted from the `X-Agent-Id` header.
    ///
    /// Discriminator for cross-role vs role-specific learning.
    /// ACL/filtering by `agent_id` is deferred. The field is captured now for
    /// forward compatibility.
    ///
    /// `None` if the header is absent (backward-compatible: existing callers without
    /// `agent_id` deserialize with `None` via `#[serde(default)]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// Response for `POST /api/v1/event-log`.
///
/// Returned with `200 OK` if ingestion succeeded.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogResponse {
    /// Number of events actually inserted.
    pub accepted_count: usize,
    /// Textual status — always `"accepted"` on success.
    pub status: String,
}
