use serde::Deserialize;

use crate::default_main;

/// Requête `vault_trace` — parité legacy vault v1.6.2 `VaultTraceArgs`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultTraceRequest {
    /// Identifiant de tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Query de traçage (tags, sections, pattern).
    pub query: String,
    /// Limite de résultats (défaut 20, max 100).
    pub limit: Option<u32>,
}
