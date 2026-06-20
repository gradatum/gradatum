use serde::Deserialize;

use crate::default_main;

/// Request body for `vault_context` — legacy vault v1.6.2 `VaultContextArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultContextRequest {
    /// Tenant identifier (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Query for which to build the LLM context.
    pub query: String,
    /// Maximum number of context tokens (default 2000, max 8000).
    pub max_tokens: Option<u32>,
    /// Section filter (optional).
    pub section: Option<String>,
}
