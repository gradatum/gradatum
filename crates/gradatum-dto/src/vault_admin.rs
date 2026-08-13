//! Vault lifecycle DTOs — **internal** administration surface.
//!
//! # API contract
//!
//! ## POST `/internal/v1/admin/vaults/create` · `/suspend` · `/delete`
//!
//! Request: [`VaultLifecycleRequest`] — the target `vault_id`, validated server-side by
//! `VaultId::parse` (parse-don't-validate). Response: **200 OK** +
//! [`VaultLifecycleResponse`]. Idempotent: replaying an operation returns
//! `changed = false`.
//!
//! These operations live EXCLUSIVELY on the internal loopback namespace, behind the admin
//! token — the same founding invariant as the delete/archive cycle. They are never mounted
//! on the public router and never exposed over MCP. The only operator entry point is the
//! `gradatum-admin vault …` CLI.
//!
//! The root vault `main` is refused on `suspend` and `delete` with **403**: suspending it
//! would brick the deployment, so the server treats it as a safety cap.

use gradatum_core::scope::VaultId;
use serde::{Deserialize, Serialize};

/// Vault lifecycle request (create / suspend / soft-delete).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultLifecycleRequest {
    /// Target vault: charset `[a-z0-9-]`, at most 64 bytes, validated by `VaultId::parse`.
    pub vault_id: VaultId,
}

impl VaultLifecycleRequest {
    /// Constructs a lifecycle request targeting `vault_id`.
    #[must_use]
    pub fn new(vault_id: VaultId) -> Self {
        Self { vault_id }
    }
}

/// Vault lifecycle response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultLifecycleResponse {
    /// Target vault.
    pub vault_id: VaultId,
    /// Tenant status after the operation: `active`, `suspended`, or `deleted`.
    pub status: String,
    /// `true` when the operation changed the state; `false` on an idempotent replay.
    pub changed: bool,
}

/// Request body for the PHYSICAL purge of a soft-deleted vault — dry-run by default.
///
/// ## POST `/internal/v1/admin/vaults/purge`
///
/// Deferred purge of a soft-deleted vault, operator-only, under the same founding
/// invariant as note hard-delete: physical destruction NEVER happens by accident.
///
/// Fail-closed. The target tenant must be in status `deleted`, otherwise **409** — an
/// active or merely suspended vault is never purgeable. An unknown tenant yields **404**,
/// and the root vault `main` yields **403**. In real mode the double confirmation
/// `confirm_vault_id == vault_id` is required, otherwise **400**.
///
/// The work is batched (`limit`, capped server-side at 500); call again while
/// `remaining > 0`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VaultPurgeRequest {
    /// Target vault — must already be soft-deleted.
    pub vault_id: VaultId,
    /// `true` (default): report what is eligible, destroying NOTHING.
    #[serde(default = "VaultPurgeRequest::default_dry_run")]
    pub dry_run: bool,
    /// Real mode: must equal `vault_id` EXACTLY (double confirmation).
    #[serde(default)]
    pub confirm_vault_id: Option<VaultId>,
    /// Maximum batch size for THIS call, clamped server-side to `[1, 500]`.
    #[serde(default = "VaultPurgeRequest::default_limit")]
    pub limit: usize,
}

impl VaultPurgeRequest {
    /// Serde default for `dry_run`: `true` — destruction is always explicitly opted into.
    fn default_dry_run() -> bool {
        true
    }

    /// Serde default for `limit`: 500, matching the server-side cap.
    fn default_limit() -> usize {
        500
    }

    /// Constructs a **dry-run** purge request for a soft-deleted `vault_id`.
    ///
    /// `dry_run` starts at `true` and `limit` at the server-side default, matching
    /// deserialization from a minimal JSON body. Set `dry_run = false` and
    /// `confirm_vault_id = Some(vault_id.clone())` to perform the real purge.
    #[must_use]
    pub fn new(vault_id: VaultId) -> Self {
        Self {
            vault_id,
            dry_run: Self::default_dry_run(),
            confirm_vault_id: None,
            limit: Self::default_limit(),
        }
    }
}

/// Vault purge response — the outcome of one batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultPurgeResponse {
    /// Target vault.
    pub vault_id: VaultId,
    /// `true` when nothing was destroyed (report only).
    pub dry_run: bool,
    /// Notes eligible at listing time (the vault total, sentinels excluded).
    pub eligible: u64,
    /// Notes destroyed in THIS batch (always 0 in dry-run).
    pub deleted: u64,
    /// Notes skipped in this batch — protected section, or a per-note error.
    pub skipped: u64,
    /// Estimated remainder after this batch (`eligible - deleted`); call again while > 0.
    pub remaining: u64,
}
