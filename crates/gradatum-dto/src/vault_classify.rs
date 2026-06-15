use serde::{Deserialize, Serialize};

use crate::default_main;

/// Request body for `vault_classify` — re-classification of an existing note.
///
/// Serialized via `bincode::serde::encode_to_vec` for the queue payload.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Serialize, Deserialize)]
pub struct VaultClassifyRequest {
    /// ULID identifier of the note to re-classify.
    pub note_id: String,
    /// Target tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
}
