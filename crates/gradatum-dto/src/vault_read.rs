use serde::Deserialize;

use crate::default_main;

/// Requête `vault_read` — parité legacy vault v1.6.2 `VaultReadArgs`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultReadRequest {
    /// Identifiant de tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Chemin de la note à lire (ex. `"decisions/my-note"`).
    pub path: String,
    /// Section cible (optionnel — si absent, lire depuis le root du vault).
    pub section: Option<String>,
}
