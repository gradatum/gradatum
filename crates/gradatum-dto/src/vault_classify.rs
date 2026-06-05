use serde::{Deserialize, Serialize};

use crate::default_main;

/// Requête `vault_classify` — re-classification d'une note existante.
///
/// Sérialisée via `bincode::serde::encode_to_vec` pour le payload de la queue.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Serialize, Deserialize)]
pub struct VaultClassifyRequest {
    /// Identifiant ULID de la note à re-classifier.
    pub note_id: String,
    /// Tenant cible (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
}
