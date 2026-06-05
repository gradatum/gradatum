use serde::{Deserialize, Serialize};

use crate::default_main;

/// Requête `vault_write` — création d'une note via la queue async.
///
/// Sérialisée via `bincode::serde::encode_to_vec` pour le payload de la queue.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Serialize, Deserialize)]
pub struct VaultWriteRequest {
    /// Titre de la note.
    pub title: String,
    /// Corps Markdown de la note.
    pub body: String,
    /// Auteur (optionnel).
    #[serde(default)]
    pub author: Option<String>,
    /// Tags initiaux (optionnel — le curator peut en ajouter d'autres).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Section suggérée (optionnel — le curator peut surclasser).
    #[serde(default)]
    pub section_hint: Option<String>,
    /// Tenant cible (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
}
