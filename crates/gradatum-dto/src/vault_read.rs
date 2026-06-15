use serde::Deserialize;

use crate::default_main;

/// Request body for `vault_read` — legacy vault v1.6.2 `VaultReadArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultReadRequest {
    /// Tenant identifier (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Path of the note to read (e.g. `"decisions/my-note"`).
    pub path: String,
    /// Target section (optional — if absent, reads from the vault root).
    pub section: Option<String>,
}
