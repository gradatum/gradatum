use serde::Deserialize;

use crate::default_main;

/// Request body for `vault_graph` — legacy vault v1.6.2 `VaultGraphArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultGraphRequest {
    /// Tenant identifier (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Starting note (path).
    pub root: String,
    /// Traversal depth (default 2, max 5).
    pub depth: Option<u32>,
    /// Include incoming links (`backlinks`).
    pub include_backlinks: Option<bool>,
}
