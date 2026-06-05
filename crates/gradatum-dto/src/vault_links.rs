use serde::Deserialize;

use crate::default_main;

/// Requête `vault_links` — parité D5 : alias thin sur vault_graph avec depth=1.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultLinksRequest {
    /// Identifiant de tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Note cible pour laquelle lister les liens.
    pub path: String,
    /// Inclure les liens entrants (`backlinks`).
    pub include_backlinks: Option<bool>,
}
