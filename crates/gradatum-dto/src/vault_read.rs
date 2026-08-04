use serde::Deserialize;

use gradatum_core::scope::TenantId;

/// Request body for `vault_read` — legacy vault v1.6.2 `VaultReadArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultReadRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Path of the note to read (e.g. `"decisions/my-note"`).
    pub path: String,
    /// Target section (optional — if absent, reads from the vault root).
    pub section: Option<String>,
    /// If `true`, the server returns a **compact** rendering instead of the full
    /// `VaultReadResponse`: an object `{ "compact": "<text>" }` bearing `path`,
    /// `section` (if any), `title` (if any), `sha256` and the note `content`, dropping
    /// only the `metadata` object and `size_bytes`.
    ///
    /// `vault_read` is **content-bound**: the note body dominates the payload, so the
    /// compaction saves a near-constant amount (JSON scaffolding + `metadata` +
    /// `size_bytes`) that is negligible next to a large note. The `sha256` needed for a
    /// later in-place update is preserved.
    ///
    /// Opt-in (default `false`): when absent, the response is **byte-for-byte identical**
    /// to the historical `VaultReadResponse`. Existing clients are unaffected.
    #[serde(default)]
    pub compact: bool,
}
