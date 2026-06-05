use serde::Deserialize;

use crate::default_main;

/// Requête `vault_search` — parité legacy vault v1.6.2 `VaultSearchArgs`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultSearchRequest {
    /// Identifiant de tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Texte de recherche full-text ou sémantique.
    pub query: String,
    /// Section à restreindre (optionnel).
    pub section: Option<String>,
    /// Nombre maximum de résultats (défaut 10, max 50).
    pub limit: Option<u32>,
    /// Phase 2.1.2 alpha.9 : si true, inclut les notes status='downgraded'
    /// dans les résultats avec score BM25 pénalisé (×0.1). Default false.
    #[serde(default)]
    pub include_downgraded: bool,
}
