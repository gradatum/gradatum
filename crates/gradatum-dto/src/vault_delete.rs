//! DTOs for the `vault_delete` endpoint — on-demand hard-delete of a single note.
//!
//! # API contract
//!
//! ## POST `/api/v1/vault_delete`
//!
//! Request: [`VaultDeleteRequest`] — **mono-note, bounded** (a single `note_id`).
//!
//! - If `dry_run = true` (default): returns **200 OK** + [`DeletePreview`] with no
//!   mutation. The preview reports the inbound `backlinks` that would become orphaned.
//! - If `dry_run = false` **AND** `confirm_ulids == [note_id]`: returns **200 OK** +
//!   [`DeleteResult`] — the note is **physically and irreversibly** removed
//!   (`.md` + `.history` + SQLite cascade + ANN + redirects). The response includes
//!   a pre-delete `backup` of the note content and the list of `backlinks_orphaned`.
//! - If `dry_run = false` AND `confirm_ulids` is missing, empty, holds more than one
//!   ULID, or does not equal `[note_id]` → **400 Bad Request**.
//!
//! ## Synchronous 200 (vs `vault_forget` async 202)
//!
//! `vault_delete` is a **bounded mono-note** operation: the cascade touches exactly
//! one note, so it runs synchronously and returns **200** with the outcome. This is
//! deliberately different from `vault_forget`, whose scope is a **batch** (topic /
//! locus / agent) and is therefore enqueued as a job returning **202 Accepted**.
//!
//! ## Protected sections (hard refusal, no bypass)
//!
//! A note whose section is in `Section::PROTECTED_DELETE`
//! (`agent-issues`, `council`, `project-map`, `identity`, `decisions`, `reasoning`)
//! can **never** be hard-deleted → **403 Forbidden**, in both dry-run and real mode.
//! There is no bypass flag, by design: these sections are the governance record, and a
//! deliberate refusal is preferred over an escape hatch that could be reached by accident.
//!
//! ## Idempotence
//!
//! Deleting a note that does not exist is a **no-op success**: 200 OK with
//! `deleted = false` and an empty `backup`.
//!
//! # Auth
//!
//! Bearer JWT required + ACL Write on the vault.

use serde::{Deserialize, Serialize};

use gradatum_core::scope::TenantId;

// ─────────────────────────────────────────────────────────────────────────────
// VaultDeleteRequest — POST /api/v1/vault_delete
// ─────────────────────────────────────────────────────────────────────────────

/// Request body for `vault_delete` — hard-delete of a **single** note.
///
/// # Two-step confirmation
///
/// 1. `POST /vault_delete { note_id, dry_run: true }` → [`DeletePreview`]
///    (section, title, orphaned backlinks — no mutation).
/// 2. Verify the preview.
/// 3. `POST /vault_delete { note_id, dry_run: false, confirm_ulids: [note_id] }`
///    → [`DeleteResult`] (note physically removed, pre-delete backup returned).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultDeleteRequest {
    /// ULID of the single note to delete.
    pub note_id: String,
    /// Dry-run (simulation without mutation) — default `true`.
    ///
    /// In dry-run mode no note is deleted; the response reports what a real
    /// delete would remove, including the orphaned inbound backlinks.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Confirmation ULIDs (required when `dry_run = false`).
    ///
    /// Must contain **exactly** the target `note_id` and nothing else. Any other
    /// value (empty, extra ULIDs, mismatched ULID) → **400 Bad Request**. Enforces
    /// the mono-note bound: a hard-delete can never target more than one note.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirm_ulids: Vec<String>,
    /// Target tenant (principal) — optional; when omitted the server resolves it
    /// from the credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
}

fn default_true() -> bool {
    true
}

// ─────────────────────────────────────────────────────────────────────────────
// DeletePreview — réponse dry-run (200)
// ─────────────────────────────────────────────────────────────────────────────

/// Response to a `vault_delete` request in dry-run mode (200 OK).
///
/// Reports the note that would be deleted and the inbound backlinks that would
/// become orphaned. No mutation is performed.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletePreview {
    /// ULID of the note that would be deleted (pass this exact value as the sole
    /// element of `confirm_ulids` to execute the real delete).
    pub note_id: String,
    /// Whether the note currently exists in the index.
    ///
    /// `false` → the delete would be a no-op (idempotent); the other fields are empty.
    pub exists: bool,
    /// Section of the note (kebab-case). Empty when `exists = false`.
    pub section: String,
    /// Markdown H1 title of the note (may be absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Inbound backlinks (ULIDs of notes linking to this one) that would become
    /// orphaned by the delete. The delete does **not** block on these — it reports them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backlinks: Vec<String>,
    /// Always `true` — marks the response as a preview.
    pub dry_run: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// DeleteResult — réponse mode réel (200)
// ─────────────────────────────────────────────────────────────────────────────

/// Response to a confirmed `vault_delete` request (200 OK).
///
/// The note has been removed from every index and is no longer reachable through the
/// API. The removal is **not** immediately irreversible: the server *archives* the note
/// — the `.md` and `.history` are moved under `.archive/` and recorded in the archive
/// registry (see `archived_path` below), with a `gc_due` set to now + the configured
/// archive retention (default 60 days). Physical destruction happens later, when the
/// registry-driven GC collects archives past their `gc_due`.
///
/// `backup` holds the note content captured **before** the cascade (recovery aid); it is
/// `None` only when the note did not exist (idempotent no-op).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteResult {
    /// ULID of the targeted note.
    pub note_id: String,
    /// `true` if a note was actually removed, `false` if it did not exist (idempotent).
    pub deleted: bool,
    /// Inbound backlinks (ULIDs) left orphaned by the delete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backlinks_orphaned: Vec<String>,
    /// Pre-delete snapshot of the note (recovery aid). `None` on idempotent no-op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<DeletedNoteBackup>,
    /// Path of the archived `.md` file, relative to the vault root.
    ///
    /// `Some` when the delete archived the note (the `.md` + `.history` were moved
    /// under `.archive/` and a registry entry was recorded). `None` on idempotent no-op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_path: Option<String>,
}

/// Snapshot of a note captured just before it is hard-deleted.
///
/// Returned in [`DeleteResult::backup`] so the caller can re-create the note if the
/// delete was a mistake. The hard-delete itself is irreversible on the server side.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedNoteBackup {
    /// Section of the deleted note (kebab-case).
    pub section: String,
    /// Markdown H1 title (may be absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Full Markdown body captured before deletion.
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_delete_request_dry_run_default() {
        let json = r#"{"note_id":"01HTEST00000000000000000AB"}"#;
        let req: VaultDeleteRequest =
            serde_json::from_str(json).expect("désérialisation VaultDeleteRequest minimal");
        assert!(req.dry_run, "dry_run doit être true par défaut");
        assert!(req.confirm_ulids.is_empty());
        assert_eq!(req.tenant_id, None);
    }

    #[test]
    fn vault_delete_request_real_mode() {
        let json = r#"{
            "note_id": "01HTEST00000000000000000AB",
            "dry_run": false,
            "confirm_ulids": ["01HTEST00000000000000000AB"]
        }"#;
        let req: VaultDeleteRequest =
            serde_json::from_str(json).expect("désérialisation VaultDeleteRequest réel");
        assert!(!req.dry_run);
        assert_eq!(req.confirm_ulids, vec!["01HTEST00000000000000000AB"]);
    }

    #[test]
    fn vault_delete_request_deny_unknown_fields() {
        let json = r#"{"note_id":"01HTEST00000000000000000AB","oops":true}"#;
        let result: Result<VaultDeleteRequest, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "deny_unknown_fields doit rejeter le champ inconnu"
        );
    }

    #[test]
    fn delete_preview_serializes() {
        let preview = DeletePreview {
            note_id: "01HTEST00000000000000000AB".to_string(),
            exists: true,
            section: "feedback".to_string(),
            title: Some("note".to_string()),
            backlinks: vec!["01HTEST00000000000000000CC".to_string()],
            dry_run: true,
        };
        let json = serde_json::to_string(&preview).expect("sérialisation DeletePreview");
        assert!(json.contains("dry_run"));
        assert!(json.contains("backlinks"));
        let back: DeletePreview =
            serde_json::from_str(&json).expect("désérialisation DeletePreview roundtrip");
        assert!(back.exists);
        assert_eq!(back.backlinks.len(), 1);
    }

    #[test]
    fn delete_result_roundtrip() {
        let res = DeleteResult {
            note_id: "01HTEST00000000000000000AB".to_string(),
            deleted: true,
            backlinks_orphaned: vec!["01HTEST00000000000000000CC".to_string()],
            backup: Some(DeletedNoteBackup {
                section: "feedback".to_string(),
                title: Some("note".to_string()),
                body: "# note\ncorps".to_string(),
            }),
            archived_path: Some(".archive/main/01HTEST00000000000000000AB.md".to_string()),
        };
        let json = serde_json::to_string(&res).expect("sérialisation DeleteResult");
        let back: DeleteResult =
            serde_json::from_str(&json).expect("désérialisation DeleteResult roundtrip");
        assert!(back.deleted);
        assert_eq!(back.backlinks_orphaned.len(), 1);
        assert!(back.backup.is_some());
        assert_eq!(
            back.archived_path.as_deref(),
            Some(".archive/main/01HTEST00000000000000000AB.md")
        );
    }

    #[test]
    fn delete_result_idempotent_noop_omits_backup() {
        let res = DeleteResult {
            note_id: "01HTEST00000000000000000AB".to_string(),
            deleted: false,
            backlinks_orphaned: vec![],
            backup: None,
            archived_path: None,
        };
        let json = serde_json::to_string(&res).expect("sérialisation DeleteResult no-op");
        assert!(!json.contains("backup"), "backup absent si no-op: {json}");
        assert!(
            !json.contains("archived_path"),
            "archived_path absent si no-op: {json}"
        );
        assert!(json.contains("\"deleted\":false"));
    }
}
