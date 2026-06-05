use serde::Deserialize;

use crate::default_main;

/// Requête `vault_context` — parité legacy vault v1.6.2 `VaultContextArgs`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultContextRequest {
    /// Identifiant de tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Query pour laquelle construire le contexte LLM.
    pub query: String,
    /// Nombre maximum de tokens de contexte (défaut 2000, max 8000).
    pub max_tokens: Option<u32>,
    /// Section à restreindre (optionnel).
    pub section: Option<String>,
}
