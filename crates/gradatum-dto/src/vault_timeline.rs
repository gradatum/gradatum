use serde::Deserialize;

use gradatum_core::scope::{TenantId, VaultId};

/// Request body for `vault_timeline` — paginated temporal read.
///
/// All fields are optional except defaults. Server sort order: `anchor_ms DESC,
/// note_id DESC`.
///
/// ## Validity windowing
///
/// The `(as_of_ms, include_expired)` pair controls temporal filtering:
///
/// | `as_of_ms` | `include_expired` | Behavior |
/// |---|---|---|
/// | absent | `false` (default) | No filter — **baseline behavior** |
/// | absent | `true` | No filter (`include_expired` has no effect without reference T) |
/// | present | `false` (default) | Strict filter: notes valid at T |
/// | present | `true` | Historical filter: notes created before T, including expired |
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultTimelineRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// vault to query in read mode (default = `tenant_id`). Max 128 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub vault_id: Option<VaultId>,
    /// `doc_kind` filter (`"Static"` / `"Event"`). Absent = all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_kind: Option<Vec<String>>,
    /// Lower bound on `anchor_ms`, inclusive (epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<i64>,
    /// Upper bound on `anchor_ms`, inclusive (epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<i64>,
    /// Maximum number of rows (default 50, clamped to max 200 server-side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Opaque pagination cursor (obtained from `next_cursor`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// As-of reference instant for validity filtering (epoch ms UTC).
    ///
    /// Activates temporal filtering. Combined with `include_expired`:
    ///
    /// - `as_of_ms=t, include_expired=false` (default): **strict filter** — notes valid at T
    ///   (`anchor_ms ≤ t AND (valid_until IS NULL OR t < valid_until)`).
    /// - `as_of_ms=t, include_expired=true`: **historical filter** — notes created before T,
    ///   including expired ones (`anchor_ms ≤ t` only). Useful for reconstructing past state.
    /// - absent: no validity filter (unchanged baseline behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of_ms: Option<i64>,
    /// Enables inclusion of expired notes **when `as_of_ms` is present**.
    ///
    /// Default `false`. No effect if `as_of_ms` is absent.
    ///
    /// `true` → historical filter: `anchor_ms ≤ t` only (notes expired at T
    /// are included — useful for reconstructing a past state).
    #[serde(default)]
    pub include_expired: bool,
    /// If `true`, the server returns a **compact** rendering instead of the full
    /// `VaultTimelineResponse`: an object `{ "compact": "<text>" }` listing each entry
    /// as `<anchor_ms> | <doc_kind> | <note_id> — <title>`, dropping `anchor_src` and
    /// the pagination `next_cursor`.
    ///
    /// Optimises token cost for LLM consumers. Because `next_cursor` is dropped, the
    /// compact form is intended for single-window reads, not cursor pagination.
    ///
    /// Opt-in (default `false`): when absent, the response is **byte-for-byte identical**
    /// to the historical `VaultTimelineResponse`. Existing clients are unaffected.
    #[serde(default)]
    pub compact: bool,
}
