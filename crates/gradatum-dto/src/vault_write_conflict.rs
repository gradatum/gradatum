//! DTO for optimistic-lock write conflicts.
//!
//! Returned in `lifecycle.result.result_note_md` (JSON-encoded) when a
//! `Job::Curate` job terminates in `JobStatus::Conflict` state.
//!
//! The client polls `GET /api/v1/jobs/{id}` → `lifecycle.status = "Conflict"` +
//! `lifecycle.result.result_note_md` contains the JSON of this DTO.
//!
//! # Emitted on optimistic-lock conflicts
//!
//! The RMW `vault_write` path produces this payload. When `expected_sha256` no longer matches
//! the note's stored content, the persist compare-and-swap refuses the write and the worker
//! marks the job `JobStatus::Conflict`, carrying this DTO. `current_sha256` holds the hash of
//! the version that "won" (the one still in the vault); `attempted_sha256` mirrors the stale
//! hash the caller supplied. A CREATE (`expected_sha256 = None`) is unconditional and never
//! conflicts, so a client must not infer from this payload's absence that a write occurred.

use serde::{Deserialize, Serialize};

/// Payload for an optimistic-lock write conflict.
///
/// Included in `JobResult.result_note_md` (JSON) when `JobStatus::Conflict`.
/// Allows the client to retrieve the current hash and decide how to resolve
/// the conflict (merge or abandon).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteConflictDto {
    /// Current SHA-256 hash of the note (64 lowercase hex chars).
    ///
    /// Hash of the version that "won" — the one that exists in the vault.
    /// The client can read it via `GET /api/v1/vault_read` to access the content.
    pub current_sha256: String,

    /// SHA-256 hash supplied by the caller (optional, 64 lowercase hex chars).
    ///
    /// Mirror of the `expected_sha256` from the original `VaultWriteRequest`.
    /// Useful for debugging and client-side conflict resolution.
    pub attempted_sha256: Option<String>,

    /// Unix timestamp in milliseconds (UTC) at which the conflict was detected.
    pub timestamp_ms: i64,
}
