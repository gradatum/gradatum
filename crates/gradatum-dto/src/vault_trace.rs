use serde::Deserialize;

use crate::default_main;

/// Request body for `vault_trace` — legacy vault v1.6.2 `VaultTraceArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultTraceRequest {
    /// Tenant identifier (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Trace query (tags, sections, pattern).
    pub query: String,
    /// Result limit (default 20, max 100).
    pub limit: Option<u32>,
}
