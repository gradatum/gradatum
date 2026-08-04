use serde::{Deserialize, Serialize};

use gradatum_core::scope::TenantId;

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
    /// Target tenant (principal) — optional; when omitted the server resolves it
    /// from the credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
    /// Expected SHA-256 of the note being overwritten (optional, hexadecimal).
    ///
    /// **Both the presence AND the value are load-bearing** — but only when `note_id` is
    /// supplied. Front-gate checks, applied synchronously before the job is enqueued:
    ///
    /// - `note_id` absent → the write creates a new note; this field is ignored.
    /// - `note_id` points at a **live** note → the field is **required**. Omitting it is
    ///   rejected with **409** (`overwrite without expected_sha256`).
    /// - `note_id` points at a **ghost** note (indexed, `.md` missing) → supplying the
    ///   field is rejected with **409** (the hash cannot be checked against any content).
    /// - `note_id` points at a **never-indexed** ULID → this is a creation: it proceeds
    ///   **unconditionally**, since there is no current content to compare against.
    /// - a syntactically invalid value is rejected with **400**, before the checks above.
    ///
    /// On the **live-note** path a genuine **compare-and-swap** follows. The write is
    /// asynchronous: the request answers **202**, and the hash is matched against the stored
    /// note inside the curate worker (`write_if_match`). A *stale* hash aborts the write and
    /// drives the job to terminal `JobStatus::Conflict`, leaving the concurrent winner
    /// intact; the conflict payload carries the winning `current_sha256`.
    ///
    /// What this does **not** give you. There is no synchronous **409** on a hash mismatch —
    /// poll `job_status` until `terminal = true`, or the conflict goes unnoticed. The
    /// compare-and-swap is not atomic in the store either: `write_if_match` reads, compares,
    /// then writes, and no storage-layer lock holds that sequence together.
    ///
    /// Its protection is also **scoped to one write path**, and it is **directional**. The
    /// hash is consulted by the curate handler only, so it guards a live note against a
    /// competing *curate* writer — not against every writer. `forget` carries no
    /// `expected_sha256` at all, and `distill` ignores the one it carries; both write
    /// unconditionally, from workers that run alongside `curate`. Which direction that
    /// leaves open matters, and it is not the intuitive one:
    ///
    /// - **A read-modify-write cannot undo a `forget`.** Not because `forget` checks a hash
    ///   — it checks none — but because it rewrites the note, which invalidates the hash a
    ///   racing write is carrying; that write then lands in `Conflict`. Content that was
    ///   forgotten stays forgotten, even under a concurrent update.
    /// - **The reverse is not guarded.** `forget` and `distill` can overwrite a concurrent
    ///   read-modify-write without consulting its hash. For `forget` that is the intent. For
    ///   `distill` it is a real gap, bounded only by that pipeline's reach: the distill cron
    ///   is disabled unless an operator turns it on, runs on a weekly schedule by default,
    ///   and enqueues nothing below a per-section pressure threshold (`distill_cron`).
    ///
    /// Read this field as a guard against a competing read-modify-write of the same note,
    /// never as a lock on the note: no deployment shape makes it one.
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
    /// Arbitrary dates (past/future) accepted by-design — no semantic bound.
    #[serde(default)]
    pub occurred_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// occurred_at present → deserialized to Some.
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
