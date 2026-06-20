use serde::Deserialize;

use crate::default_main;

/// Request body for `vault_list` — legacy vault v1.6.2 `VaultListArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultListRequest {
    /// Tenant identifier (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Section to list (optional — if absent, lists all sections).
    pub section: Option<String>,
    /// Optional glob filter pattern (e.g. `"decisions/*"`).
    pub pattern: Option<String>,
    /// Maximum number of entries (default 100, max 1000).
    pub limit: Option<u32>,
    /// Pagination cursor (opaque token).
    pub cursor: Option<String>,
}
