use serde::Deserialize;

use gradatum_core::scope::TenantId;

/// Request body for `vault_links` — thin alias over `vault_graph` with `depth=1`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultLinksRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Target note for which to list links.
    pub path: String,
    /// Include incoming links (`backlinks`).
    pub include_backlinks: Option<bool>,
}
