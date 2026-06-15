use serde::Deserialize;

use crate::default_main;

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
    /// Tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// vault to query in read mode (default = `tenant_id`). Max 128 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_id: Option<String>,
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
}
