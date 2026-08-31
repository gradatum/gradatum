use serde::Deserialize;

use gradatum_core::scope::TenantId;

/// Request body for `vault_list` — legacy vault v1.6.2 `VaultListArgs` parity.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultListRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Section to list (optional — if absent, lists all sections).
    pub section: Option<String>,
    /// NOT IMPLEMENTED — the server reads this field and discards it, silently. Setting
    /// it filters nothing: the response is exactly the one you would get without it. Do
    /// not use it to narrow a listing; filter on `section`, or filter client-side.
    pub pattern: Option<String>,
    /// Maximum number of entries. Default 20, clamped to the range 1..=200 — a larger
    /// value is silently lowered to 200, not rejected.
    pub limit: Option<u32>,
    /// Pagination cursor (opaque token).
    pub cursor: Option<String>,
    /// Filter on the `kind` role of a project-map card (`FIX`, `FEATURE`,
    /// `ENHANCEMENT`, `TASK`). Absent or `null`: no filter — the routing to
    /// `list_notes` is unchanged. The value is the canonical SCREAMING_SNAKE wire form.
    pub role_kind: Option<String>,
    /// Filter on the `status` role of a project-map card (`OPEN`, `IN_PROGRESS`, `BLOCKED`,
    /// `DONE`, `OBSOLETE`, `BRAINSTORMING`). Not to be confused with the note's lifecycle
    /// status (`live`, `downgraded`): two distinct notions, two distinct columns.
    pub role_status: Option<String>,
}
