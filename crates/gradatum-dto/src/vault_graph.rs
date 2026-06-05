use serde::Deserialize;

use crate::default_main;

/// Requête `vault_graph` — parité legacy vault v1.6.2 `VaultGraphArgs`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultGraphRequest {
    /// Identifiant de tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Note de départ (chemin).
    pub root: String,
    /// Profondeur de traversée (défaut 2, max 5).
    pub depth: Option<u32>,
    /// Inclure les liens entrants (`backlinks`).
    pub include_backlinks: Option<bool>,
}
