//! Operator sub-commands for the delete/archive lifecycle.
//!
//! `gradatum-admin delete <id>` · `archives list` · `archives purge <id>` ·
//! `archives restore <id>` (or `--from/--to [--section]` for a whole range). All of them
//! go through [`AdminClient`] — loopback endpoint plus an admin token read from a file —
//! and never touch `index.db` directly, since the running server owns it.
//!
//! Every mutation is a dry-run preview by default; `--execute` switches to the real mode.
//! That is a client-side convenience, not the actual safety net: the server independently
//! requires `confirm_ulids == [id]` before mutating anything.

use std::path::PathBuf;

use anyhow::Context as _;
use gradatum_dto::{
    VaultArchivesListRequest, VaultArchivesPurgeRequest, VaultArchivesRestoreRequest,
    VaultArchivesRestoreResult, VaultDeleteRequest, VaultLifecycleRequest, VaultPurgeRequest,
};

use crate::admin_client::{AdminClient, DEFAULT_ADMIN_TOKEN_FILE, DEFAULT_ADMIN_URL};

/// Resolves the admin API URL, defaulting to the internal loopback endpoint.
fn resolve_url(url: Option<String>) -> String {
    url.unwrap_or_else(|| DEFAULT_ADMIN_URL.to_string())
}

/// Resolves the admin token file path, defaulting to `/etc/gradatum/admin.token`.
fn resolve_token_file(p: Option<PathBuf>) -> PathBuf {
    p.unwrap_or_else(|| PathBuf::from(DEFAULT_ADMIN_TOKEN_FILE))
}

/// Connection parameters shared by every admin sub-command.
pub struct Conn {
    /// Admin API URL; defaults to [`DEFAULT_ADMIN_URL`].
    pub url: Option<String>,
    /// Admin token file; defaults to [`DEFAULT_ADMIN_TOKEN_FILE`].
    pub token_file: Option<PathBuf>,
}

impl Conn {
    fn client(self) -> anyhow::Result<AdminClient> {
        AdminClient::new(&resolve_url(self.url), &resolve_token_file(self.token_file))
    }
}

/// `gradatum-admin delete <id>` — on-demand deletion, which archives rather than destroys.
///
/// Dry-run by default; `--execute` performs the archival, moving the `.md` and `.history`
/// files under `.archive/`. The operation stays reversible through `archives restore`
/// until the retention garbage collector destroys the archive.
///
/// # Errors
///
/// The server is unreachable, the token file cannot be read, or the server answers with
/// an error status.
pub async fn run_delete(
    conn: Conn,
    id: String,
    tenant: Option<String>,
    execute: bool,
) -> anyhow::Result<()> {
    let client = conn.client()?;
    let mut req = VaultDeleteRequest::new(id.clone());
    req.dry_run = !execute;
    req.confirm_ulids = if execute {
        vec![id.clone()]
    } else {
        Vec::new()
    };
    req.tenant_id = tenant.map(|t| t.into());
    let resp = client
        .delete(&req)
        .await
        .context("admin delete call failed")?;
    let pretty = serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string());
    if execute {
        println!("delete (REAL) — note archived:\n{pretty}");
    } else {
        println!("delete (DRY-RUN, no mutation) — re-run with --execute to archive:\n{pretty}");
    }
    Ok(())
}

/// Filters for the archive listing.
pub struct ArchivesListArgs {
    /// Owning vault to filter on; `None` means every vault.
    pub vault: Option<String>,
    /// Section to filter on (kebab-case).
    pub section: Option<String>,
    /// Lower bound: `archived_at >= since_ms` (epoch milliseconds).
    pub since_ms: Option<i64>,
    /// Upper bound: `archived_at <= until_ms` (epoch milliseconds).
    pub until_ms: Option<i64>,
    /// Include archives that have already been destroyed.
    pub include_gc: bool,
    /// Include archives that have already been restored.
    pub include_restored: bool,
    /// Maximum number of rows; defaults to 50 and is clamped to 500 by the server.
    pub limit: Option<usize>,
    /// Pagination offset.
    pub offset: Option<usize>,
    /// Target tenant — `None` = derived from the bearer token (A1).
    pub tenant: Option<String>,
}

/// `gradatum-admin archives list` — lists the archive registry.
///
/// # Errors
///
/// The server is unreachable, the token file cannot be read, or the server answers with
/// an error status.
pub async fn run_archives_list(conn: Conn, args: ArchivesListArgs) -> anyhow::Result<()> {
    let client = conn.client()?;
    let mut req = VaultArchivesListRequest::default();
    req.vault_filter = args.vault;
    req.section = args.section;
    req.since_ms = args.since_ms;
    req.until_ms = args.until_ms;
    req.include_gc = args.include_gc;
    req.include_restored = args.include_restored;
    req.limit = args.limit.unwrap_or(50);
    req.offset = args.offset.unwrap_or(0);
    req.tenant_id = args.tenant.map(|t| t.into());
    let resp = client
        .archives_list(&req)
        .await
        .context("admin archives list call failed")?;
    println!(
        "{} archive(s) (limit {}, offset {}) :",
        resp.count, resp.limit, resp.offset
    );
    for e in &resp.entries {
        let state = match (e.gc_at, e.restored_at) {
            (Some(_), _) => "destroyed",
            (_, Some(_)) => "restored",
            _ => "active",
        };
        println!(
            "  {} @{} [{}] {} — {} · archived_by={} · gc_due={} · {}",
            e.note_id,
            e.vault_id,
            e.section,
            e.title.as_deref().unwrap_or("(untitled)"),
            state,
            e.archived_by.as_deref().unwrap_or("?"),
            e.gc_due,
            e.archive_path,
        );
    }
    Ok(())
}

/// `gradatum-admin archives purge <id>` — destroys an archive ahead of its retention date.
///
/// Dry-run by default; `--execute` destroys the archive for good. This cannot be undone.
///
/// # Errors
///
/// The server is unreachable, the token file cannot be read, or the server answers with
/// an error status.
pub async fn run_archives_purge(
    conn: Conn,
    id: String,
    tenant: Option<String>,
    execute: bool,
) -> anyhow::Result<()> {
    let client = conn.client()?;
    let mut req = VaultArchivesPurgeRequest::new(id.clone());
    req.dry_run = !execute;
    req.confirm_ulids = if execute {
        vec![id.clone()]
    } else {
        Vec::new()
    };
    req.tenant_id = tenant.map(|t| t.into());
    let resp = client
        .archives_purge(&req)
        .await
        .context("admin archives purge call failed")?;

    match resp.archive {
        Some(a) => println!(
            "archive cible : {} [{}] {} → {}",
            a.note_id,
            a.section,
            a.title.as_deref().unwrap_or("(untitled)"),
            a.archive_path
        ),
        None => println!("no active archive for {} (no-op).", resp.note_id),
    }
    if resp.dry_run {
        println!("purge (DRY-RUN) — re-run with --execute to destroy (IRREVERSIBLE).");
    } else if resp.purged {
        println!("purge (REAL) — archive destroyed, registry trace kept (gc_at set).");
    } else {
        println!("purge (REAL) — nothing to destroy (idempotent).");
    }
    Ok(())
}

/// Restores a single archive through the internal endpoint, in dry-run or real mode.
async fn restore_one(
    client: &AdminClient,
    id: &str,
    tenant: Option<&str>,
    execute: bool,
) -> anyhow::Result<VaultArchivesRestoreResult> {
    let mut req = VaultArchivesRestoreRequest::new(id.to_string());
    req.dry_run = !execute;
    req.confirm_ulids = if execute {
        vec![id.to_string()]
    } else {
        Vec::new()
    };
    req.tenant_id = tenant.map(|t| t.to_string().into());
    client
        .archives_restore(&req)
        .await
        .context("admin archives restore call failed")
}

/// Prints the outcome of a single restore.
fn print_restore_result(resp: &VaultArchivesRestoreResult) {
    match &resp.archive {
        Some(a) => println!(
            "archive cible : {} [{}] {} → {}",
            a.note_id,
            a.section,
            a.title.as_deref().unwrap_or("(untitled)"),
            a.archive_path
        ),
        None => println!("no active archive for {} (no-op).", resp.note_id),
    }
    if resp.dry_run {
        println!(
            "restore (DRY-RUN) — re-run with --execute to restore into quarantine (pending-review)."
        );
    } else if resp.restored {
        println!(
            "restore (REAL) — note restored in {} → {} (promotion to live via curator).",
            resp.status.as_deref().unwrap_or("?"),
            resp.restored_path.as_deref().unwrap_or("?"),
        );
    } else {
        println!("restore (REAL) — nothing to restore.");
    }
}

/// `gradatum-admin archives restore <id>` — restores an archive into quarantine.
///
/// Dry-run by default; `--execute` performs the restore. The note comes back with status
/// `pending-review`, that is, in the curator's quarantine: promotion to `live` always goes
/// through the curator and never happens automatically.
///
/// # Errors
///
/// The server is unreachable, the token file cannot be read, or the server answers with an
/// error status (`404` no active archive, `409` ULID collision).
pub async fn run_archives_restore_one(
    conn: Conn,
    id: String,
    tenant: Option<String>,
    execute: bool,
) -> anyhow::Result<()> {
    let client = conn.client()?;
    let resp = restore_one(&client, &id, tenant.as_deref(), execute).await?;
    print_restore_result(&resp);
    Ok(())
}

/// Filters for a restore over a date range.
pub struct ArchivesRestoreRangeArgs {
    /// Lower bound: `archived_at >= from_ms` (epoch milliseconds).
    pub from_ms: Option<i64>,
    /// Upper bound: `archived_at <= to_ms` (epoch milliseconds).
    pub to_ms: Option<i64>,
    /// Section to filter on (kebab-case); `None` means every section.
    pub section: Option<String>,
    /// Target tenant — `None` = derived from the bearer token (A1).
    pub tenant: Option<String>,
    /// Perform the restore for real; otherwise only a dry-run preview is printed.
    pub execute: bool,
}

/// `gradatum-admin archives restore --from --to [--section]` — restores a whole range.
///
/// Lists the **active** archives of the range (`archived_at` between `from` and `to`,
/// optional section filter, at most the 500 rows the server allows), then restores them
/// one by one — each call re-validates the per-note confirmation server-side. Dry-run by
/// default, which previews the range without mutating anything.
///
/// # Errors
///
/// The server is unreachable, the token file cannot be read, or the server answers with an
/// error status on the listing call. Per-note restore failures are reported line by line
/// and do not abort the range, since each note is independent.
pub async fn run_archives_restore_range(
    conn: Conn,
    args: ArchivesRestoreRangeArgs,
) -> anyhow::Result<()> {
    let client = conn.client()?;
    let mut list_req = VaultArchivesListRequest::default();
    list_req.section = args.section;
    list_req.since_ms = args.from_ms;
    list_req.until_ms = args.to_ms;
    list_req.limit = 500;
    list_req.tenant_id = args.tenant.clone().map(|t| t.into());
    let list = client
        .archives_list(&list_req)
        .await
        .context("listing of the range to restore failed")?;

    if list.entries.is_empty() {
        println!("no active archive in the range — nothing to restore.");
        return Ok(());
    }
    println!("{} active archive(s) in the range:", list.count);
    for e in &list.entries {
        println!(
            "  {} [{}] {} — archived_at={}",
            e.note_id,
            e.section,
            e.title.as_deref().unwrap_or("(untitled)"),
            e.archived_at
        );
    }
    if !args.execute {
        println!(
            "restore range (DRY-RUN) — re-run with --execute to restore the {} note(s) into quarantine.",
            list.count
        );
        return Ok(());
    }

    let mut restored = 0usize;
    for e in &list.entries {
        match restore_one(&client, &e.note_id, args.tenant.as_deref(), true).await {
            Ok(r) if r.restored => {
                restored += 1;
                println!("  ✓ {} → {}", e.note_id, r.status.as_deref().unwrap_or("?"));
            }
            Ok(_) => println!("  · {} : nothing to restore", e.note_id),
            Err(err) => println!("  ✗ {} : {err}", e.note_id),
        }
    }
    println!(
        "restore range (REAL) — {restored}/{} note(s) restored into quarantine.",
        list.count
    );
    Ok(())
}

/// A vault lifecycle operation.
#[derive(Debug, Clone, Copy)]
pub enum VaultLifecycleOp {
    /// Provisions the vault — active tenant plus a self write grant. Idempotent.
    Create,
    /// Freezes the vault: operations are rejected immediately, and the freeze is reversible.
    Suspend,
    /// Soft deletion: operations are rejected immediately, physical purge is deferred to jobs.
    SoftDelete,
}

/// `gradatum-admin vault create|suspend|soft-delete <vault_id>` — vault lifecycle.
///
/// The server validates the identifier, protects the `main` vault with a `403`, and
/// answers idempotently: replaying a call returns `changed: false`.
///
/// # Errors
///
/// The server is unreachable, the token file cannot be read, or the server answers with an
/// error status (`400` malformed vault id, `403` on the `main` vault, `404` unknown vault).
pub async fn run_vault_lifecycle(
    conn: Conn,
    op: VaultLifecycleOp,
    vault_id: String,
) -> anyhow::Result<()> {
    let client = conn.client()?;
    let req = VaultLifecycleRequest::new(vault_id.into());
    let resp = match op {
        VaultLifecycleOp::Create => client.vault_create(&req).await,
        VaultLifecycleOp::Suspend => client.vault_suspend(&req).await,
        VaultLifecycleOp::SoftDelete => client.vault_soft_delete(&req).await,
    }
    .context("vault lifecycle call failed")?;
    println!(
        "vault '{}' → status={} changed={}",
        resp.vault_id, resp.status, resp.changed
    );
    Ok(())
}

/// `gradatum-admin vault purge <vault_id>` — physically removes a soft-deleted vault.
///
/// Dry-run by default; `--execute` destroys data for good. Each run processes a bounded
/// batch, so re-run the command as long as the reported `remaining` count is above zero.
/// The server is fail-closed: it requires the vault to be in status `deleted` (`409`
/// otherwise) and always refuses the `main` vault (`403`).
///
/// # Errors
///
/// The server is unreachable, the token file cannot be read, or the server answers with an
/// error status (`400` malformed vault id or missing confirmation, `403` on the `main`
/// vault, `404` unknown vault, `409` vault not soft-deleted).
pub async fn run_vault_purge(
    conn: Conn,
    vault_id: String,
    execute: bool,
    limit: usize,
) -> anyhow::Result<()> {
    let client = conn.client()?;
    let mut req = VaultPurgeRequest::new(vault_id.clone().into());
    req.dry_run = !execute;
    req.confirm_vault_id = if execute { Some(vault_id.into()) } else { None };
    req.limit = limit;
    let resp = client
        .vault_purge(&req)
        .await
        .context("vault purge call failed")?;
    println!(
        "vault '{}' — eligible={} deleted={} skipped={} remaining={}",
        resp.vault_id, resp.eligible, resp.deleted, resp.skipped, resp.remaining
    );
    if resp.dry_run {
        println!("purge (DRY-RUN) — re-run with --execute to destroy (IRREVERSIBLE).");
    } else if resp.remaining > 0 {
        println!("purge (REAL) — batch done, re-run to continue (remaining > 0).");
    } else {
        println!("purge (REAL) — vault emptied (tombstone 'deleted' kept in tenants).");
    }
    Ok(())
}
