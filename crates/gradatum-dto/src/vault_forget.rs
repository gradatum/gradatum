//! DTOs for the `vault_forget` endpoint — semantic forgetting of notes.
//!
//! # API contract
//!
//! ## POST `/api/v1/vault_forget`
//!
//! Request: [`VaultForgetRequest`].
//!
//! - If `dry_run = true` (default): returns **200 OK** + [`ForgetPreview`] with no mutation.
//! - If `dry_run = false` **AND** `confirm_ulids` = exact ULIDs from a prior preview:
//!   returns **202 Accepted** + `ForgetJobResponse` (job enqueued — mutation applied
//!   asynchronously by the worker). `ForgetJobResponse` is defined in
//!   `gradatum-server` and is not a public L0 DTO.
//! - If `dry_run = false` AND `confirm_ulids` is missing or does not match the ULIDs
//!   computed at execution time → **400 Bad Request**.
//!
//! ## GET `/api/v1/vault/forgotten`
//!
//! Returns [`ForgottenListResponse`] — paginated list of forgotten notes.
//!
//! ## POST `/api/v1/vault/unforgot/{ulid}`
//!
//! Restores a forgotten note. Returns **200 OK** + [`UnforgotResponse`].
//! Consistency note: the SQLite index is updated synchronously (immediate).
//! The frontmatter YAML of the `.md` file on disk is resynchronized on the next
//! vault access (cache miss or write) — a residual `forgotten` flag may linger in
//! direct YAML reads until that point.

use serde::{Deserialize, Serialize};

use gradatum_core::scope::TenantId;

/// Maximum byte length of the `forgotten_by` actor field.
///
/// Safety cap (DoS by storage amplification): `forgotten_by` is persisted once per
/// note in the batch (SQLite `forgotten_by` column + per-note frontmatter YAML), so an
/// unbounded actor string is amplified across every targeted note. 512 bytes is far
/// above any legitimate agent/user identifier while bounding the worst case. Enforced
/// at the HTTP boundary by the `/vault_forget` handler → **400 Bad Request** on
/// overflow (deterministic, fail-closed).
pub const MAX_FORGOTTEN_BY_LEN: usize = 512;

// ─────────────────────────────────────────────────────────────────────────────
// ForgetScopeDto — wire representation du scope
// ─────────────────────────────────────────────────────────────────────────────

/// Forget resolution scope — HTTP wire representation.
///
/// L0 DTO: same structure as `ForgetScope` (gradatum-core) but without a dependency
/// on the domain crate (intentional decoupling).
///
/// The 3 variants are identified by the `type` field:
/// - `"topic"`: FTS search.
/// - `"locus"`: locus prefix.
/// - `"agent"`: all notes from an agent.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ForgetScopeDto {
    /// Resolution by FTS search over an optional vault.
    Topic {
        /// FTS query (e.g. `"secrets api-key"`).
        query: String,
        /// Target vault (optional — `None` = `"main"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vault: Option<String>,
        /// Result cap (default 50, max 200).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// Resolution by locus prefix.
    Locus {
        /// Target vault.
        vault: String,
        /// locus prefix (e.g. `"inbox/old/"`).
        locus: String,
    },
    /// Resolution by agent — all notes from an `agent_id`.
    Agent {
        /// Agent identifier (the `author_id` column).
        agent_id: String,
        /// List of target vaults (`[]` → `["main"]`).
        #[serde(default)]
        vaults: Vec<String>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// VaultForgetRequest — POST /api/v1/vault_forget
// ─────────────────────────────────────────────────────────────────────────────

/// Request body for `vault_forget` — semantic forgetting of notes.
///
/// # Dry-run required (default `true`)
///
/// A forget without a prior preview is not possible in real mode:
/// `dry_run = false` requires `confirm_ulids` to exactly match the ULIDs
/// from a previous preview. Any discrepancy → **400 Bad Request**.
///
/// # Recommended workflow
///
/// 1. `POST /vault_forget { scope, dry_run: true }` → `ForgetPreview { ulids, count, excluded }`
/// 2. Verify the preview (exact list of notes that will be forgotten).
/// 3. `POST /vault_forget { scope, dry_run: false, confirm_ulids: <ulids from preview> }`
///    → `ForgetJobResponse { job_id, status: "queued", poll_url, preview }` (202 Accepted —
///    mutations applied asynchronously by the worker)
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultForgetRequest {
    /// Resolution scope — determines the target notes.
    pub scope: ForgetScopeDto,
    /// Dry-run (simulation without mutation) — default `true`.
    ///
    /// In dry-run mode, no note is modified. The response contains the exact list
    /// of ULIDs that would be forgotten, plus exclusions (protected sections).
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Actor triggering the forget (recorded as `forgotten_by`).
    ///
    /// Optional — `None` if not provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgotten_by: Option<String>,
    /// Confirmation ULIDs (required when `dry_run = false`).
    ///
    /// Must exactly match the ULIDs returned by the preview.
    /// Any discrepancy (addition, omission, different order) → **400 Bad Request**.
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
// ForgetPreview — réponse dry-run
// ─────────────────────────────────────────────────────────────────────────────

/// Response to a `vault_forget` request in dry-run mode.
///
/// Contains the exact list of ULIDs that would be forgotten, along with exclusions
/// (notes from protected sections excluded from the batch).
///
/// These `ulids` must be passed as `confirm_ulids` in the real execution request.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetPreview {
    /// ULIDs of the notes that would be forgotten (exact list for confirmation).
    pub ulids: Vec<String>,
    /// Number of targeted notes.
    pub count: usize,
    /// Notes excluded because they belong to protected sections (agent-issues, council).
    ///
    /// The job does not fail on exclusions — they are reported here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<ExcludedNote>,
    /// Indicates the response is a preview — always `true` here.
    pub dry_run: bool,
}

/// Note excluded from the forget batch because it belongs to a protected section.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludedNote {
    /// ULID of the excluded note.
    pub ulid: String,
    /// Section of the note (e.g. `"agent-issues"`, `"council"`).
    pub section: String,
    /// Reason for exclusion.
    pub reason: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// ForgottenListResponse — GET /api/v1/vault/forgotten
// ─────────────────────────────────────────────────────────────────────────────

/// Response for `GET /api/v1/vault/forgotten` — paginated list of forgotten notes.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgottenListResponse {
    /// Forgotten notes.
    pub notes: Vec<ForgottenNoteEntry>,
    /// Total number of forgotten notes in the vault.
    pub total: usize,
    /// Cursor for the next page (ULID — exclusive). `None` if this is the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Entry for a forgotten note in the list.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgottenNoteEntry {
    /// ULID of the note.
    pub ulid: String,
    /// Title of the note (optional — absent if not indexed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Section of the note (kebab-case).
    pub section: String,
    /// Epoch ms timestamp of the forgotten marking.
    pub forgotten_at: i64,
    /// Actor that triggered the forget (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgotten_by: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// UnforgotResponse — POST /api/v1/vault/unforgot/{ulid}
// ─────────────────────────────────────────────────────────────────────────────

/// Response for `POST /api/v1/vault/unforgot/{ulid}` — restores a forgotten note.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnforgotResponse {
    /// ULID of the restored note.
    pub ulid: String,
    /// Status after restoration: always `"restored"`.
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_forget_request_dry_run_default() {
        let json = r#"{"scope":{"type":"topic","query":"test"}}"#;
        let req: VaultForgetRequest = serde_json::from_str(json)
            .expect("désérialisation VaultForgetRequest scope topic minimal");
        assert!(req.dry_run, "dry_run doit être true par défaut");
        assert!(req.confirm_ulids.is_empty());
        assert_eq!(req.tenant_id, None);
    }

    #[test]
    fn vault_forget_request_locus_scope() {
        let json = r#"{
            "scope": {"type":"locus","vault":"main","locus":"inbox/old/"},
            "dry_run": false,
            "confirm_ulids": ["01HTEST00000000000000000AB"]
        }"#;
        let req: VaultForgetRequest =
            serde_json::from_str(json).expect("désérialisation VaultForgetRequest scope locus");
        assert!(!req.dry_run);
        assert_eq!(req.confirm_ulids.len(), 1);
        assert!(
            matches!(req.scope, ForgetScopeDto::Locus { ref locus, .. } if locus == "inbox/old/")
        );
    }

    #[test]
    fn vault_forget_request_agent_scope() {
        let json = r#"{
            "scope": {"type":"agent","agent_id":"claude-agent","vaults":["main"]}
        }"#;
        let req: VaultForgetRequest =
            serde_json::from_str(json).expect("désérialisation VaultForgetRequest scope agent");
        assert!(req.dry_run, "dry_run par défaut");
        assert!(
            matches!(req.scope, ForgetScopeDto::Agent { ref agent_id, .. } if agent_id == "claude-agent")
        );
    }

    #[test]
    fn forget_preview_serializes() {
        let preview = ForgetPreview {
            ulids: vec!["01HTEST00000000000000000AB".to_string()],
            count: 1,
            excluded: vec![ExcludedNote {
                ulid: "01HTEST00000000000000000CC".to_string(),
                section: "agent-issues".to_string(),
                reason: "section protégée".to_string(),
            }],
            dry_run: true,
        };
        let json = serde_json::to_string(&preview).expect("sérialisation ForgetPreview");
        assert!(json.contains("dry_run"));
        assert!(json.contains("excluded"));
        let back: ForgetPreview =
            serde_json::from_str(&json).expect("désérialisation ForgetPreview roundtrip");
        assert_eq!(back.count, 1);
        assert_eq!(back.excluded.len(), 1);
    }

    #[test]
    fn forgotten_list_response_roundtrip() {
        let resp = ForgottenListResponse {
            notes: vec![ForgottenNoteEntry {
                ulid: "01HTEST00000000000000000AB".to_string(),
                title: Some("note oubliée".to_string()),
                section: "decisions".to_string(),
                forgotten_at: 1_700_000_000_000,
                forgotten_by: Some("operator-1".to_string()),
            }],
            total: 1,
            next_cursor: None,
        };
        let json = serde_json::to_string(&resp).expect("sérialisation ForgottenListResponse");
        let back: ForgottenListResponse =
            serde_json::from_str(&json).expect("désérialisation ForgottenListResponse roundtrip");
        assert_eq!(back.total, 1);
        assert!(back.next_cursor.is_none());
    }

    #[test]
    fn unforgot_response_roundtrip() {
        let resp = UnforgotResponse {
            ulid: "01HTEST00000000000000000AB".to_string(),
            status: "restored".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("sérialisation UnforgotResponse");
        let back: UnforgotResponse =
            serde_json::from_str(&json).expect("désérialisation UnforgotResponse roundtrip");
        assert_eq!(
            back.status, "restored",
            "status doit être 'restored' (contrat API unforgot)"
        );
    }

    #[test]
    fn vault_forget_request_deny_unknown_fields() {
        let json = r#"{"scope":{"type":"topic","query":"test"},"unknown_field":"oops"}"#;
        let result: Result<VaultForgetRequest, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "deny_unknown_fields doit rejeter le champ inconnu"
        );
    }
}
