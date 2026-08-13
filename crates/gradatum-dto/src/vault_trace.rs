use serde::Deserialize;

use gradatum_core::scope::TenantId;

/// Request body for `vault_trace` — legacy vault v1.6.2 `VaultTraceArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultTraceRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Trace query (tags, sections, pattern).
    pub query: String,
    /// Result limit (default 20, max 100).
    pub limit: Option<u32>,
}

impl VaultTraceRequest {
    /// Constructs a trace request with the mandatory `query`; `tenant_id` and
    /// `limit` default to `None`.
    #[must_use]
    pub fn new(query: String) -> Self {
        Self {
            tenant_id: None,
            query,
            limit: None,
        }
    }
}
