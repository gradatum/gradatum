use serde::Deserialize;

use gradatum_core::scope::TenantId;

/// Request body for `vault_graph` — legacy vault v1.6.2 `VaultGraphArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultGraphRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Starting note (path).
    pub root: String,
    /// Traversal depth (default 2, max 5).
    pub depth: Option<u32>,
    /// Include incoming links (`backlinks`).
    pub include_backlinks: Option<bool>,
}

impl VaultGraphRequest {
    /// Constructs a graph request rooted at `root`; `tenant_id`, `depth` and
    /// `include_backlinks` default to `None`.
    #[must_use]
    pub fn new(root: String) -> Self {
        Self {
            tenant_id: None,
            root,
            depth: None,
            include_backlinks: None,
        }
    }
}
