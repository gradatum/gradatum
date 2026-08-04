//! `gradatum-admin jobs {list,get,cancel,dlq}` — job queue introspection.
//!
//! ## Sub-commands
//!
//! ```text
//! gradatum-admin jobs list   --root /var/lib/gradatum [--status pending] [--kind Curate] [--limit 50]
//! gradatum-admin jobs get    --root /var/lib/gradatum <id>
//! gradatum-admin jobs cancel --root /var/lib/gradatum <id>
//! gradatum-admin jobs dlq    --root /var/lib/gradatum [--replay <id>] [--replay-all]
//! gradatum-admin jobs dlq    --root /var/lib/gradatum --prune [--older-than <Nd>] [--apply]
//! ```
//!
//! ## Direct SQLite access
//!
//! Commands access the SQLite database (WAL) directly, bypassing the HTTP server.
//! The database path is derived from `--root`: `<root>/db/queue.sqlite`.
//!
//! ## Database path
//!
//! - SQLite queue (WAL): `<root>/db/queue.sqlite`

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use gradatum_core::{JobFilter, JobRecord, JobStatus, QueueStore, paths::queue_db_path};
use gradatum_db_sqlite::{SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use sqlx::SqlitePool;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Sous-commandes
// ─────────────────────────────────────────────────────────────────────────────

/// Sub-commands of `jobs`.
#[derive(Debug, Subcommand)]
pub enum JobsCmd {
    /// Lists jobs with optional filters.
    List(JobsListArgs),
    /// Displays details of a job by its ULID.
    Get(JobsGetArgs),
    /// Cancels a job (no-op if `Running`).
    Cancel(JobsCancelArgs),
    /// Dead Letter Queue operations — list and replay.
    Dlq(JobsDlqArgs),
}

/// Arguments for `jobs list`.
#[derive(Debug, Args)]
pub struct JobsListArgs {
    /// Gradatum root directory.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// Filter by status (pending, running, done, failed, dlq, cancelled).
    #[arg(long)]
    pub status: Option<String>,

    /// Filter by kind (Curate, Embed, Summarize, …).
    #[arg(long)]
    pub kind: Option<String>,

    /// Maximum number of results (default 50, max 200).
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

/// Arguments for `jobs get`.
#[derive(Debug, Args)]
pub struct JobsGetArgs {
    /// Gradatum root directory.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// Job ULID.
    pub id: String,
}

/// Arguments for `jobs cancel`.
#[derive(Debug, Args)]
pub struct JobsCancelArgs {
    /// Gradatum root directory.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// ULID of the job to cancel.
    pub id: String,
}

/// Arguments for `jobs dlq`.
#[derive(Debug, Args)]
pub struct JobsDlqArgs {
    /// Gradatum root directory.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// Replays a single DLQ job (ULID) — resets it to `Pending`.
    #[arg(long)]
    pub replay: Option<String>,

    /// Replays all DLQ jobs — resets all to `Pending`.
    #[arg(long)]
    pub replay_all: bool,

    /// Permanently deletes DLQ jobs — irreversible.
    ///
    /// Dry-run by default (prints the count of jobs that would be deleted without
    /// deleting anything). Add `--apply` to execute the actual deletion.
    /// Mutually exclusive with `--replay` / `--replay-all`.
    #[arg(long, conflicts_with_all = ["replay", "replay_all"])]
    pub prune: bool,

    /// Restricts pruning to DLQ jobs older than `<N>d` days (e.g. `30d`).
    ///
    /// Without a value, all DLQ jobs are targeted. Has no effect without `--prune`.
    #[arg(long, value_name = "Nd")]
    pub older_than: Option<String>,

    /// Confirms actual pruning execution (otherwise dry-run).
    #[arg(long)]
    pub apply: bool,

    /// Maximum number of jobs to list (default 50).
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Entrée principale
// ─────────────────────────────────────────────────────────────────────────────

/// Main dispatch for `gradatum-admin jobs`.
pub async fn run(cmd: JobsCmd) -> Result<()> {
    match cmd {
        JobsCmd::List(args) => jobs_list(args).await,
        JobsCmd::Get(args) => jobs_get(args).await,
        JobsCmd::Cancel(args) => jobs_cancel(args).await,
        JobsCmd::Dlq(args) => jobs_dlq(args).await,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Opens the SQLite queue pool from `<root>/db/queue.sqlite`.
///
/// Applies WAL pragmas and runs migrations.
async fn open_queue_pool(root: &std::path::Path) -> Result<SqlitePool> {
    // SSOT : chemin via helper canonique — jamais root.join(...) manuel.
    let db_path = queue_db_path(root);
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = SqlitePool::connect(&url)
        .await
        .with_context(|| format!("cannot open the SQLite queue: {}", db_path.display()))?;
    apply_sqlite_pragmas(&pool)
        .await
        .context("queue WAL pragmas error")?;
    run_migrations(&pool)
        .await
        .context("queue migrations error")?;
    Ok(pool)
}

/// Formats a `JobRecord` as a single summary line for `list`.
fn format_record_short(r: &JobRecord) -> String {
    format!(
        "{id}  {status:<12}  {kind:<20}  class={class:?}  prio={prio}  created={created}",
        id = r.id,
        status = format!("{:?}", r.lifecycle.status),
        kind = format!("{:?}", r.spec.kind)
            .chars()
            .take(20)
            .collect::<String>(),
        class = r.spec.class,
        prio = r.spec.priority.as_u8(),
        created = r.lifecycle.created_at.format("%Y-%m-%dT%H:%M:%SZ"),
    )
}

/// Parses a string into `JobStatus` (case-insensitive).
fn parse_status(s: &str) -> Result<JobStatus> {
    match s.to_lowercase().as_str() {
        "pending" => Ok(JobStatus::Pending),
        "running" => Ok(JobStatus::Running),
        "waiting" => Ok(JobStatus::Waiting),
        "done" => Ok(JobStatus::Done),
        "failed" => Ok(JobStatus::Failed),
        "dlq" => Ok(JobStatus::DLQ),
        "cancelled" | "canceled" => Ok(JobStatus::Cancelled),
        other => anyhow::bail!(
            "unknown status: '{}' (valid values: pending, running, waiting, done, failed, dlq, cancelled)",
            other
        ),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────────────

/// `jobs list` — lists jobs with optional filters.
async fn jobs_list(args: JobsListArgs) -> Result<()> {
    let pool = open_queue_pool(&args.root).await?;
    let store = SqliteQueueStore::new(pool);

    let status = args.status.as_deref().map(parse_status).transpose()?;
    let limit = args.limit.clamp(1, 200);

    let filter = JobFilter {
        status,
        kind: args.kind.clone(),
        limit,
        ..Default::default()
    };

    let records = store
        .list(filter)
        .await
        .context("error while listing jobs")?;

    if records.is_empty() {
        println!("No job found.");
        return Ok(());
    }

    println!(
        "{} job(s) — filtres: status={:?} kind={:?} limit={}",
        records.len(),
        args.status,
        args.kind,
        limit
    );
    println!("{}", "─".repeat(90));
    for r in &records {
        println!("{}", format_record_short(r));
    }

    Ok(())
}

/// `jobs get` — displays details of a job.
async fn jobs_get(args: JobsGetArgs) -> Result<()> {
    let id = args
        .id
        .parse::<Ulid>()
        .with_context(|| format!("invalid ULID: '{}'", args.id))?;

    let pool = open_queue_pool(&args.root).await?;
    let store = SqliteQueueStore::new(pool);

    match store.get(id, None).await.context("get job error")? {
        None => {
            eprintln!("Job {} not found.", id);
            std::process::exit(1);
        }
        Some(record) => {
            // Pretty-print the full JobRecord as JSON
            let json = serde_json::to_string_pretty(&record)
                .context("JobRecord JSON serialization error")?;
            println!("{}", json);
        }
    }

    Ok(())
}

/// `jobs cancel` — cancels a job (no-op if `Running`).
async fn jobs_cancel(args: JobsCancelArgs) -> Result<()> {
    let id = args
        .id
        .parse::<Ulid>()
        .with_context(|| format!("invalid ULID: '{}'", args.id))?;

    let pool = open_queue_pool(&args.root).await?;
    let store = SqliteQueueStore::new(pool);

    // Check current status before cancellation
    let record = match store.get(id, None).await.context("get job error")? {
        None => {
            eprintln!("Job {} not found.", id);
            std::process::exit(1);
        }
        Some(r) => r,
    };

    match record.lifecycle.status {
        JobStatus::Running => {
            eprintln!("Cannot cancel job {} : Running status (409 Conflict).", id);
            eprintln!("Wait for execution to finish or use `fail_dlq` if the worker is dead.");
            std::process::exit(1);
        }
        JobStatus::Done | JobStatus::DLQ | JobStatus::Cancelled => {
            println!(
                "Job {} already terminal (status={:?}) — idempotent cancellation.",
                id, record.lifecycle.status
            );
            return Ok(());
        }
        _ => {}
    }

    store
        .cancel(id, None)
        .await
        .context("error while cancelling the job")?;

    println!("Job {} cancelled (status=Cancelled).", id);
    Ok(())
}

/// `jobs dlq` — lists DLQ jobs and optionally replays them.
async fn jobs_dlq(args: JobsDlqArgs) -> Result<()> {
    let pool = open_queue_pool(&args.root).await?;
    let store = SqliteQueueStore::new(pool.clone());

    // Prune (permanent deletion) — dry-run by default, --apply to execute.
    if args.prune {
        return jobs_dlq_prune(&store, args.older_than.as_deref(), args.apply).await;
    }

    // Individual replay
    if let Some(ref id_str) = args.replay {
        let id = id_str
            .parse::<Ulid>()
            .with_context(|| format!("invalid ULID: '{}'", id_str))?;
        return replay_single(&store, &pool, id).await;
    }

    // List DLQ jobs
    let filter = JobFilter {
        status: Some(JobStatus::DLQ),
        limit: args.limit.clamp(1, 200),
        ..Default::default()
    };
    let dlq_jobs = store.list(filter).await.context("DLQ listing error")?;

    if dlq_jobs.is_empty() {
        println!("DLQ empty — no job in the Dead Letter Queue.");
        return Ok(());
    }

    println!("{} job(s) en DLQ :", dlq_jobs.len());
    println!("{}", "─".repeat(90));
    for r in &dlq_jobs {
        let last_err = r.retry.last_error.as_deref().unwrap_or("(no detail)");
        println!(
            "{}  retries={}/{}  last_error={}",
            format_record_short(r),
            r.retry.count,
            r.retry.max,
            &last_err[..last_err.len().min(80)],
        );
    }

    // Replay all if requested
    if args.replay_all {
        println!();
        println!("Replay de {} job(s) DLQ en Pending...", dlq_jobs.len());
        let mut replayed = 0usize;
        let mut errors = 0usize;
        for r in &dlq_jobs {
            match replay_single(&store, &pool, r.id).await {
                Ok(()) => replayed += 1,
                Err(e) => {
                    eprintln!("  REPLAY ERROR {} : {e}", r.id);
                    errors += 1;
                }
            }
        }
        println!("Replay complete: {} OK, {} error(s).", replayed, errors);
    }

    Ok(())
}

/// Replays a single DLQ job by resetting it to `Pending` via a direct SQL update.
///
/// A direct SQL query is used for DLQ replay, outside of any DAG chaining.
async fn replay_single(_store: &SqliteQueueStore, pool: &SqlitePool, id: Ulid) -> Result<()> {
    let id_str = id.to_string();
    let result = sqlx::query(
        r#"
        UPDATE gradatum_jobs
        SET status        = 'Pending',
            lease_until   = NULL,
            scheduled_at  = datetime('now'),
            -- Reset the attempt counter: without reset, the replayed job would
            -- have attempt_count >= max_attempts and be immediately sent back
            -- to DLQ by promote_retries on the next sweep (30s).
            attempt_count = 0,
            last_error    = NULL
        WHERE id = ?
          AND status = 'DLQ'
        "#,
    )
    .bind(&id_str)
    .execute(pool)
    .await
    .with_context(|| format!("DLQ replay error job {}", id))?;

    if result.rows_affected() == 0 {
        anyhow::bail!("Job {} not found in DLQ (status ≠ DLQ or unknown ID)", id);
    }

    println!("  Job {} remis en Pending (replay DLQ OK).", id);
    Ok(())
}

/// Parses an age window `<N>d` (e.g. `"30d"`) into a `DateTime<Utc>` cutoff.
///
/// Returns `Utc::now() - N days`. Only the `d` (days) suffix is supported.
fn parse_older_than(spec: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let spec = spec.trim();
    let days_str = spec.strip_suffix('d').with_context(|| {
        format!("--older-than: expected format '<N>d' (e.g. '30d'), received '{spec}'")
    })?;
    let days: i64 = days_str
        .parse()
        .with_context(|| format!("--older-than: invalid number of days in '{spec}'"))?;
    if days < 0 {
        anyhow::bail!("--older-than: the number of days must be positive (received '{spec}')");
    }
    Ok(chrono::Utc::now() - chrono::Duration::days(days))
}

/// `jobs dlq --prune` — permanently deletes DLQ jobs (irreversible).
///
/// Dry-run by default: counts and displays jobs that would be deleted without
/// deleting anything. `--apply` executes the actual deletion via `delete_dlq_jobs`.
/// `older_than` (`<N>d`) restricts pruning to DLQ jobs older than N days.
async fn jobs_dlq_prune(
    store: &SqliteQueueStore,
    older_than: Option<&str>,
    apply: bool,
) -> Result<()> {
    let cutoff = older_than.map(parse_older_than).transpose()?;

    // Exact count of targeted DLQ jobs (dry-run) via a dedicated `COUNT(*)` query,
    // using the SAME WHERE clause as the DELETE in `delete_dlq_jobs`.
    // Replaces the former `list(limit: 200)` approach, which under-counted beyond
    // 200 DLQ entries and could early-return "nothing to delete" with `--older-than`.
    let targeted = store
        .count_dlq_jobs(cutoff)
        .await
        .context("DLQ count error (prune)")?;

    let scope = match older_than {
        Some(spec) => format!("older than {spec}"),
        None => "all".to_string(),
    };

    if targeted == 0 {
        println!("Prune DLQ ({scope}): no job targeted — nothing to delete.");
        return Ok(());
    }

    if !apply {
        println!(
            "Prune DLQ ({scope}) — DRY-RUN: {targeted} job(s) would be PERMANENTLY deleted.\n\
             Re-run with --apply to perform the deletion (irreversible)."
        );
        return Ok(());
    }

    // Actual execution — permanent deletion via the store (DELETE WHERE status='DLQ').
    let deleted = store
        .delete_dlq_jobs(cutoff)
        .await
        .context("DLQ deletion error (prune)")?;
    println!("Prune DLQ ({scope}): {deleted} job(s) permanently deleted.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_older_than_valid_days() {
        let cutoff = parse_older_than("30d").expect("30d doit parser");
        let now = chrono::Utc::now();
        let expected = now - chrono::Duration::days(30);
        // Tolérance large (le now du parse diffère de quelques ms du now du test).
        let delta = (cutoff - expected).num_seconds().abs();
        assert!(delta < 5, "cutoff ~ now-30j (delta={delta}s)");
    }

    #[test]
    fn parse_older_than_zero_is_now() {
        let cutoff = parse_older_than("0d").expect("0d doit parser");
        let delta = (chrono::Utc::now() - cutoff).num_seconds().abs();
        assert!(delta < 5, "0d → ~now (delta={delta}s)");
    }

    #[test]
    fn parse_older_than_rejects_missing_suffix() {
        assert!(parse_older_than("30").is_err(), "sans suffixe 'd' → erreur");
    }

    #[test]
    fn parse_older_than_rejects_non_numeric() {
        assert!(parse_older_than("abcd").is_err(), "non numérique → erreur");
    }

    #[test]
    fn parse_older_than_rejects_negative() {
        assert!(parse_older_than("-5d").is_err(), "négatif → erreur");
    }
}
