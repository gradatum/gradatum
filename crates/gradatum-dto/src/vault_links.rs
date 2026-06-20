use serde::Deserialize;

use crate::default_main;

/// Request body for `vault_links` — thin alias over `vault_graph` with `depth=1`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultLinksRequest {
    /// Tenant identifier (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Target note for which to list links.
    pub path: String,
    /// Include incoming links (`backlinks`).
    pub include_backlinks: Option<bool>,
}
