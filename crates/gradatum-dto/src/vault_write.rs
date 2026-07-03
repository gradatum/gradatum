use serde::{Deserialize, Serialize};

use crate::default_main;

/// Request body for `vault_write` — creates a note via the async queue.
///
/// The **LIVE queue path** (`SqliteQueueStore`) serializes this struct via `serde_json`.
/// The bincode serialization (`bincode::serde::encode_to_vec`) is used only by the
/// legacy `dispatch.rs` dispatcher and does not apply to the active job pipeline.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Serialize, Deserialize)]
pub struct VaultWriteRequest {
    /// Title of the note.
    pub title: String,
    /// Markdown body of the note.
    pub body: String,
    /// Author (optional).
    #[serde(default)]
    pub author: Option<String>,
    /// Initial tags (optional — the curator may add more).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Suggested section (optional — the curator may override).
    #[serde(default)]
    pub section_hint: Option<String>,
    /// Target tenant (default `"main"`).
    #[serde(default = "default_main")]
    pub tenant_id: String,
    /// Expected SHA-256 hash for optimistic locking (optional, hexadecimal).
    ///
    /// When present, the worker checks that the current hash of the note matches
    /// before writing. On mismatch: terminal job `Conflict` (note not overwritten).
    /// `None` = unconditional write (backward-compatible).
    ///
    /// Format: 64 lowercase hexadecimal characters (SHA-256 = 32 bytes).
    /// Example: `"a3f1c2d4..."`
    #[serde(default)]
    pub expected_sha256: Option<String>,
    /// Pre-allocated ULID — honored by the legacy `SqliteQueue` dispatcher if present
    /// and parseable as a valid ULID.
    ///
    /// `None` = backward-compatible behavior: the dispatcher generates a fresh ULID.
    ///
    /// ## Bincode field-order invariant
    ///
    /// This field is at **position 8** (last) — AFTER `tenant_id` (pos 6) and
    /// `expected_sha256` (pos 7). Bincode v2 (`config::standard()`) is positional:
    /// declaration order determines serialization order.
    /// Never move this field without updating all encoders.
    #[serde(default)]
    pub note_id: Option<String>,
    /// Temporal anchor — event date of the note (ISO 8601 UTC or YYYY-MM-DD).
    ///
    /// When present and parseable, the worker sets `anchor_src = occurred_at` and
    /// `anchor_ms` to this date's epoch milliseconds, instead of the creation time.
    ///
    /// Absent or `null` → `anchor_src = created` (backward-compatible behaviour).
    ///
    /// ## Accepted formats
    ///
    /// - ISO 8601 / RFC 3339 with time: `"2026-01-15T10:00:00Z"`
    /// - Date-only YYYY-MM-DD → start of day UTC: `"2026-01-15"`
    ///
    /// An unparseable value is rejected by the server with **400 InvalidInput**.
    ///
    /// Dates arbitraires (passé/futur) acceptées by-design — pas de borne sémantique.
    #[serde(default)]
    pub occurred_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// occurred_at présent → désérialisé en Some.
    #[test]
    fn vault_write_request_occurred_at_present_parses_to_some() {
        let json = r#"{"title":"T","body":"B","occurred_at":"2026-01-15"}"#;
        let req: VaultWriteRequest = serde_json::from_str(json).expect("parsing VaultWriteRequest");
        assert_eq!(req.occurred_at, Some("2026-01-15".to_string()));
    }

    /// occurred_at absent → None (backward-compat).
    #[test]
    fn vault_write_request_occurred_at_absent_defaults_to_none() {
        let json = r#"{"title":"T","body":"B"}"#;
        let req: VaultWriteRequest = serde_json::from_str(json).expect("parsing VaultWriteRequest");
        assert_eq!(req.occurred_at, None);
    }

    /// occurred_at ISO 8601 complet (RFC 3339) → Some.
    #[test]
    fn vault_write_request_occurred_at_iso8601_full_parses_to_some() {
        let json = r#"{"title":"T","body":"B","occurred_at":"2026-01-15T10:00:00Z"}"#;
        let req: VaultWriteRequest = serde_json::from_str(json).expect("parsing VaultWriteRequest");
        assert_eq!(req.occurred_at, Some("2026-01-15T10:00:00Z".to_string()));
    }
}
