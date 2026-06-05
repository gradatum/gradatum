use serde::Deserialize;

use crate::default_main;

/// Requête `vault_list` — parité legacy vault v1.6.2 `VaultListArgs`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultListRequest {
    /// Identifiant de tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Section à lister (optionnel — si absent, liste toutes les sections).
    pub section: Option<String>,
    /// Pattern de filtre glob optionnel (ex. `"decisions/*"`).
    pub pattern: Option<String>,
    /// Nombre maximum d'entrées (défaut 100, max 1000).
    pub limit: Option<u32>,
    /// Curseur de pagination (token opaque).
    pub cursor: Option<String>,
}
