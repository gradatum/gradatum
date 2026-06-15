use serde::{Deserialize, Serialize};

use crate::default_main;

/// Request body for `POST /api/v1/session-log/trace` (append-only session trace insert).
///
/// Wire contract for the append-only `session_trace` endpoint. No `agent_id` field:
/// agent identity is derived **server-side** from the JWT `sub` claim — never from
/// the request body.
///
/// `deny_unknown_fields`: an unexpected field (e.g. a client-injected `agent_id`)
/// → 422 at deserialization, never accepted silently.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTraceRequest {
    /// Tenant (default `"main"`). For explicitness only: ACL identity ALWAYS uses
    /// the `tenant_id` from the JWT. If provided AND different from the JWT → 422
    /// in the handler (no silent client/server divergence).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Session ULID. Omitted = server-generated. Provided = validated ULID format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Epoch ms timestamp of the action.
    pub ts_ms: i64,
    /// Action type (`plan` | `edit` | `tool-call` | `decision` | `verdict` | `deploy` | …).
    /// Bounded to ≤64 chars by the handler.
    pub action_type: String,
    /// Action target. Bounded to ≤512 chars by the handler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Short intent description. Bounded to ≤200 chars by the handler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// Outcome (`success` | `failure` | `partial`). Validated as an enum by the handler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Reference (sha7 | ULID | section/ULID). Regex-validated by the handler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    // No `agent_id` field: derived from the JWT `sub` claim server-side.
}

/// Response for `POST /api/v1/session-log/trace`.
///
/// Returns the `rowid` of the inserted row and the effective `session_id`
/// (useful to the client when it was server-generated).
#[derive(Debug, Serialize)]
pub struct SessionTraceResponse {
    /// Internal identifier (SQLite rowid) of the inserted row.
    pub id: i64,
    /// Effective session ULID (server-generated if the client omitted it).
    pub session_id: String,
}
