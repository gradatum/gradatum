use serde::{Deserialize, Serialize};

use crate::default_main;

/// Request body for `vault_write` — creates a note via the async queue.
///
/// Serialized via `bincode::serde::encode_to_vec` for the queue payload.
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
}
