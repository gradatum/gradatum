//! DTOs for `vault_archives_list` — **read-only** listing of the archive registry.
//!
//! # API contract
//!
//! ## POST `/api/v1/vault_archives_list`
//!
//! Request: [`VaultArchivesListRequest`] — filters + pagination, **no mutation, no
//! action parameter**. Returns **200 OK** + [`VaultArchivesListResponse`].
//!
//! This surface is **strictly read-only**: it lets an agent (via MCP) or the operator
//! (via the public API) *see* the archived notes and **prepare** the corresponding
//! `gradatum-admin archives …` commands. The mutating operations of the archive cycle
//! (delete/restore/purge) are **never** exposed here — they live only in the internal
//! loopback namespace, reachable exclusively by the operator CLI. That separation is a
//! founding invariant of the archive cycle: destruction never happens by accident, and
//! never at an agent's initiative.
//!
//! # Auth
//!
//! Bearer JWT required + ACL Read on the vault (same tier as `vault_search`).

use serde::{Deserialize, Serialize};

use crate::default_main_vault;
use gradatum_core::scope::{TenantId, VaultId};

// ─────────────────────────────────────────────────────────────────────────────
// VaultArchivesListRequest — POST /api/v1/vault_archives_list
// ─────────────────────────────────────────────────────────────────────────────

/// Request body for `vault_archives_list` — read-only, filterable, paginated.
///
/// Every field is optional. With no filter, the default lists the **active** archives
/// (neither garbage-collected nor restored), most recent first.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultArchivesListRequest {
    /// Filter by owning vault. `None` = no vault restriction (all vaults are listed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_filter: Option<String>,
    /// Filter by canonical section (kebab-case). `None` = all sections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Lower bound on the archival instant (`archived_at >= since_ms`, epoch ms UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
    /// Upper bound on the archival instant (`archived_at <= until_ms`, epoch ms UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<i64>,
    /// Include archives already physically destroyed by the GC (`gc_at IS NOT NULL`).
    #[serde(default)]
    pub include_gc: bool,
    /// Include archives already restored (`restored_at IS NOT NULL`).
    #[serde(default)]
    pub include_restored: bool,
    /// Maximum number of rows (server-capped at 500). Default 50.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Pagination offset. Default 0.
    #[serde(default)]
    pub offset: usize,
    /// Target tenant (principal) — optional; when omitted the server resolves it
    /// from the credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
}

fn default_limit() -> usize {
    50
}

// ─────────────────────────────────────────────────────────────────────────────
// VaultArchivesListResponse — 200 OK
// ─────────────────────────────────────────────────────────────────────────────

/// Response to a `vault_archives_list` request (200 OK).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultArchivesListResponse {
    /// The archive registry rows matching the filter (most recent first).
    pub entries: Vec<ArchiveEntryDto>,
    /// Effective `limit` applied (after the server cap).
    pub limit: usize,
    /// Effective `offset` applied.
    pub offset: usize,
    /// Number of rows returned in this page (`entries.len()`).
    pub count: usize,
}

/// Wire representation of one archive registry row.
///
/// An archive is **active** (recoverable) when both `gc_at` and `restored_at` are absent.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveEntryDto {
    /// ULID of the archived note.
    pub note_id: String,
    /// Owning vault of the archive (mirror of `notes.vault_id`).
    #[serde(default = "default_main_vault")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub vault_id: VaultId,
    /// Original canonical section (kebab-case).
    pub section: String,
    /// H1 title at archival time, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Original locus (sub-directory) — `None` = tenant root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_locus: Option<String>,
    /// Archive `.md` path relative to the vault root.
    pub archive_path: String,
    /// Archival instant (epoch ms UTC).
    pub archived_at: i64,
    /// `sub` of the token that triggered the archival, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_by: Option<String>,
    /// Retention deadline (epoch ms) beyond which the GC destroys the archive.
    pub gc_due: i64,
    /// Physical destruction instant (epoch ms) — `None` if the archive still exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_at: Option<i64>,
    /// Restoration instant (epoch ms) — `None` if never restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_at: Option<i64>,
}

// ─────────────────────────────────────────────────────────────────────────────
// VaultArchivesPurgeRequest / Result — purge à la demande (admin interne uniquement)
// ─────────────────────────────────────────────────────────────────────────────

/// Request body for the internal admin `archives purge` — destroys an archive **before**
/// its retention deadline. Two-step confirmation, single note.
///
/// This is **never** a public/MCP surface: it is reachable only via the internal loopback
/// admin namespace (operator CLI). Real execution requires `dry_run=false` **and**
/// `confirm_ulids == [note_id]`.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultArchivesPurgeRequest {
    /// ULID of the archived note to purge.
    pub note_id: String,
    /// Dry-run (default `true`): preview the archive that would be destroyed, no mutation.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Confirmation ULIDs (required when `dry_run=false`): must equal exactly `[note_id]`.
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

/// Response to a `archives purge` request (200 OK).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultArchivesPurgeResult {
    /// ULID of the targeted note.
    pub note_id: String,
    /// `true` if `dry_run` (preview) — no destruction happened.
    pub dry_run: bool,
    /// `true` if an active archive was destroyed (real mode) — `false` in dry-run or when
    /// no active archive existed (idempotent no-op).
    pub purged: bool,
    /// The active archive entry that was (or would be) purged — `None` if none exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveEntryDto>,
}

// ─────────────────────────────────────────────────────────────────────────────
// VaultArchivesRestoreRequest / Result — restauration en quarantaine (admin interne)
// ─────────────────────────────────────────────────────────────────────────────

/// Request body for the internal admin `archives restore` — moves an archived note
/// back to the vault in **quarantine**. Two-step confirmation, single note.
///
/// This is **never** a public/MCP surface: reachable only via the internal loopback
/// admin namespace (operator CLI). Real execution requires `dry_run=false` **and**
/// `confirm_ulids == [note_id]`.
///
/// The restored note re-enters with status **`pending-review`** (quarantine): it is not
/// visible/live by default and re-joins the curator pipeline. Promotion back to `live`
/// goes through the normal curator review — never automatically at restore time.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultArchivesRestoreRequest {
    /// ULID of the archived note to restore.
    pub note_id: String,
    /// Dry-run (default `true`): preview the archive that would be restored, no mutation.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Confirmation ULIDs (required when `dry_run=false`): must equal exactly `[note_id]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirm_ulids: Vec<String>,
    /// Target tenant (principal) — optional; when omitted the server resolves it
    /// from the credential identity (JWT/API-key), never `"main"` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub tenant_id: Option<TenantId>,
}

/// Response to an `archives restore` request (200 OK).
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultArchivesRestoreResult {
    /// ULID of the targeted note.
    pub note_id: String,
    /// `true` if `dry_run` (preview) — no restoration happened.
    pub dry_run: bool,
    /// `true` if the note was restored (real mode) — `false` in dry-run.
    pub restored: bool,
    /// Resulting note status after restoration (`"pending-review"`) — `None` in dry-run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Restored `.md` path relative to the vault root — `None` in dry-run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_path: Option<String>,
    /// The active archive entry that was (or would be) restored — `None` if none exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveEntryDto>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_request_dry_run_default() {
        let req: VaultArchivesPurgeRequest =
            serde_json::from_str(r#"{"note_id":"01HTEST00000000000000000AB"}"#)
                .expect("désérialisation purge minimal");
        assert!(req.dry_run, "dry_run défaut true");
        assert!(req.confirm_ulids.is_empty());
        assert_eq!(req.tenant_id, None);
    }

    #[test]
    fn purge_request_deny_unknown_fields() {
        let result: Result<VaultArchivesPurgeRequest, _> =
            serde_json::from_str(r#"{"note_id":"x","oops":true}"#);
        assert!(result.is_err(), "deny_unknown_fields rejette champ inconnu");
    }

    #[test]
    fn restore_request_dry_run_default() {
        let req: VaultArchivesRestoreRequest =
            serde_json::from_str(r#"{"note_id":"01HTEST00000000000000000AB"}"#)
                .expect("désérialisation restore minimal");
        assert!(req.dry_run, "dry_run défaut true");
        assert!(req.confirm_ulids.is_empty());
        assert_eq!(req.tenant_id, None);
    }

    #[test]
    fn restore_request_deny_unknown_fields() {
        let result: Result<VaultArchivesRestoreRequest, _> =
            serde_json::from_str(r#"{"note_id":"x","oops":true}"#);
        assert!(result.is_err(), "deny_unknown_fields rejette champ inconnu");
    }

    #[test]
    fn request_defaults() {
        let req: VaultArchivesListRequest =
            serde_json::from_str("{}").expect("désérialisation VaultArchivesListRequest vide");
        assert_eq!(req.limit, 50);
        assert_eq!(req.offset, 0);
        assert!(!req.include_gc);
        assert!(!req.include_restored);
        assert!(req.section.is_none());
        assert_eq!(req.tenant_id, None);
    }

    #[test]
    fn request_deny_unknown_fields() {
        // Notamment : un champ `action` est refusé — la surface est lecture seule stricte.
        let json = r#"{"action":"restore"}"#;
        let result: Result<VaultArchivesListRequest, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "deny_unknown_fields doit rejeter tout champ hors filtre (dont action)"
        );
    }

    #[test]
    fn request_filters_parse() {
        let json = r#"{
            "section": "feedback",
            "since_ms": 1000,
            "until_ms": 2000,
            "include_gc": true,
            "include_restored": true,
            "limit": 10,
            "offset": 5
        }"#;
        let req: VaultArchivesListRequest =
            serde_json::from_str(json).expect("désérialisation filtres");
        assert_eq!(req.section.as_deref(), Some("feedback"));
        assert_eq!(req.since_ms, Some(1000));
        assert_eq!(req.until_ms, Some(2000));
        assert!(req.include_gc);
        assert!(req.include_restored);
        assert_eq!(req.limit, 10);
        assert_eq!(req.offset, 5);
    }

    #[test]
    fn response_roundtrip() {
        let resp = VaultArchivesListResponse {
            entries: vec![ArchiveEntryDto {
                note_id: "01HTEST00000000000000000AB".to_string(),
                vault_id: VaultId::new("main"),
                section: "feedback".to_string(),
                title: Some("note".to_string()),
                original_locus: None,
                archive_path: ".archive/main/01HTEST00000000000000000AB.md".to_string(),
                archived_at: 1000,
                archived_by: Some("admin".to_string()),
                gc_due: 5_184_001_000,
                gc_at: None,
                restored_at: None,
            }],
            limit: 50,
            offset: 0,
            count: 1,
        };
        let json = serde_json::to_string(&resp).expect("sérialisation réponse");
        let back: VaultArchivesListResponse =
            serde_json::from_str(&json).expect("désérialisation réponse roundtrip");
        assert_eq!(back.count, 1);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].archived_by.as_deref(), Some("admin"));
        // Une archive active omet gc_at/restored_at du JSON.
        assert!(!json.contains("gc_at"));
        assert!(!json.contains("restored_at"));
    }
}
