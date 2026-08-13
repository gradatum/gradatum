use serde::{Deserialize, Serialize};

use gradatum_core::scope::TenantId;

/// Request body for `vault_history` — lists the copy-on-write snapshots of a note.
///
/// Returns the Unix millisecond timestamps of snapshots stored in
/// `.history/<note_id>/` for the note identified by `note_id`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultHistoryRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Note ULID (e.g. `"01JTEXAMPLE"`).
    pub note_id: String,
}

impl VaultHistoryRequest {
    /// Constructs a history request for `note_id`; `tenant_id` defaults to `None`.
    #[must_use]
    pub fn new(note_id: String) -> Self {
        Self {
            tenant_id: None,
            note_id,
        }
    }
}

/// Request body for `vault_history_get` — reads a specific historical snapshot.
///
/// Returns the note content at the time of snapshot `ts_ms`.
/// `ts_ms` must be a timestamp obtained from `vault_history`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultHistoryGetRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Note ULID.
    pub note_id: String,
    /// Unix ms timestamp of the snapshot (obtained from `vault_history`).
    pub ts_ms: i64,
}

impl VaultHistoryGetRequest {
    /// Constructs a snapshot-read request for `note_id` at `ts_ms`; `tenant_id`
    /// defaults to `None`.
    #[must_use]
    pub fn new(note_id: String, ts_ms: i64) -> Self {
        Self {
            tenant_id: None,
            note_id,
            ts_ms,
        }
    }
}

/// Request body for `vault_restore` — restores a note from a snapshot.
///
/// Writes snapshot `ts_ms` as the new current version.
/// Triggers a copy-on-write (the previous current version is saved to `.history/`).
/// Returns the SHA-256 hex hash of the restored version.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultRestoreRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// ULID of the note to restore.
    pub note_id: String,
    /// Unix ms timestamp of the snapshot to restore.
    pub ts_ms: i64,
}

impl VaultRestoreRequest {
    /// Constructs a restore request for `note_id` at snapshot `ts_ms`; `tenant_id`
    /// defaults to `None`.
    #[must_use]
    pub fn new(note_id: String, ts_ms: i64) -> Self {
        Self {
            tenant_id: None,
            note_id,
            ts_ms,
        }
    }
}

/// Request body for `vault_diff` — raw line-by-line diff between two versions.
///
/// `a` and `b` are Unix ms timestamps (obtained from `vault_history`)
/// or the literal string `"current"` for the current version.
/// Returns a list of lines prefixed with ` ` (common), `-` (removed), `+` (added).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultDiffRequest {
    /// Tenant (principal) — optional; when omitted the server resolves it from the
    /// credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Note ULID.
    pub note_id: String,
    /// Version A: Unix ms timestamp or `"current"`.
    pub a: String,
    /// Version B: Unix ms timestamp or `"current"`.
    pub b: String,
}

impl VaultDiffRequest {
    /// Constructs a diff request between versions `a` and `b` of `note_id`;
    /// `tenant_id` defaults to `None`.
    #[must_use]
    pub fn new(note_id: String, a: String, b: String) -> Self {
        Self {
            tenant_id: None,
            note_id,
            a,
            b,
        }
    }
}

/// Response for `vault_history`.
#[derive(Debug, Serialize)]
pub struct VaultHistoryResponse {
    /// Unix ms timestamps of snapshots, sorted ascending (oldest first).
    pub versions: Vec<i64>,
    /// Number of available snapshots.
    pub count: usize,
}

/// Response for `vault_history_get` — content of a snapshot.
#[derive(Debug, Serialize)]
pub struct VaultHistoryGetResponse {
    /// Note ULID.
    pub note_id: String,
    /// Unix ms timestamp of the snapshot.
    pub ts_ms: i64,
    /// Markdown body of the snapshot.
    pub body: String,
    /// Section of the note at the time of the snapshot.
    pub section: String,
}

/// Response for `vault_restore`.
#[derive(Debug, Serialize)]
pub struct VaultRestoreResponse {
    /// Note ULID.
    pub note_id: String,
    /// Unix ms timestamp of the restored snapshot.
    pub ts_ms: i64,
    /// SHA-256 hex hash of the restored version.
    pub content_hash: String,
}

/// Response for `vault_diff`.
#[derive(Debug, Serialize)]
pub struct VaultDiffResponse {
    /// Diff lines (prefix ` ` / `-` / `+`).
    pub lines: Vec<String>,
    /// Total number of lines (reserved for future pagination).
    pub count: usize,
}
