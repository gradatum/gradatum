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
    let pool = SqlitePool::connect(&url).await.with_context(|| {
        format!(
            "impossible d'ouvrir la queue SQLite : {}",
            db_path.display()
        )
    })?;
    apply_sqlite_pragmas(&pool)
        .await
        .context("erreur pragmas WAL queue")?;
    run_migrations(&pool)
        .await
        .context("erreur migrations queue")?;
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
            "statut inconnu : '{}' (valeurs valides : pending, running, waiting, done, failed, dlq, cancelled)",
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
        .context("erreur lors du listing des jobs")?;

    if records.is_empty() {
        println!("Aucun job trouvé.");
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
        .with_context(|| format!("ULID invalide : '{}'", args.id))?;

    let pool = open_queue_pool(&args.root).await?;
    let store = SqliteQueueStore::new(pool);

    match store.get(id).await.context("erreur get job")? {
        None => {
            eprintln!("Job {} introuvable.", id);
            std::process::exit(1);
        }
        Some(record) => {
            // Pretty-print the full JobRecord as JSON
            let json = serde_json::to_string_pretty(&record)
                .context("erreur sérialisation JSON JobRecord")?;
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
        .with_context(|| format!("ULID invalide : '{}'", args.id))?;

    let pool = open_queue_pool(&args.root).await?;
    let store = SqliteQueueStore::new(pool);

    // Check current status before cancellation
    let record = match store.get(id).await.context("erreur get job")? {
        None => {
            eprintln!("Job {} introuvable.", id);
            std::process::exit(1);
        }
        Some(r) => r,
    };

    match record.lifecycle.status {
        JobStatus::Running => {
            eprintln!(
                "Impossible d'annuler le job {} : statut Running (409 Conflict).",
                id
            );
            eprintln!("Attendre la fin d'exécution ou utiliser `fail_dlq` si le worker est mort.");
            std::process::exit(1);
        }
        JobStatus::Done | JobStatus::DLQ | JobStatus::Cancelled => {
            println!(
                "Job {} déjà terminal (statut={:?}) — annulation idempotente.",
                id, record.lifecycle.status
            );
            return Ok(());
        }
        _ => {}
    }

    store
        .cancel(id)
        .await
        .context("erreur lors de l'annulation du job")?;

    println!("Job {} annulé (statut=Cancelled).", id);
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
            .with_context(|| format!("ULID invalide : '{}'", id_str))?;
        return replay_single(&store, &pool, id).await;
    }

    // List DLQ jobs
    let filter = JobFilter {
        status: Some(JobStatus::DLQ),
        limit: args.limit.clamp(1, 200),
        ..Default::default()
    };
    let dlq_jobs = store.list(filter).await.context("erreur listing DLQ")?;

    if dlq_jobs.is_empty() {
        println!("DLQ vide — aucun job en Dead Letter Queue.");
        return Ok(());
    }

    println!("{} job(s) en DLQ :", dlq_jobs.len());
    println!("{}", "─".repeat(90));
    for r in &dlq_jobs {
        let last_err = r.retry.last_error.as_deref().unwrap_or("(sans détail)");
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
                    eprintln!("  ERREUR replay {} : {e}", r.id);
                    errors += 1;
                }
            }
        }
        println!("Replay terminé : {} OK, {} erreur(s).", replayed, errors);
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
            -- Réinitialise le compteur de tentatives : sans reset, le job replayed
            -- aurait attempt_count >= max_attempts et serait immédiatement renvoyé
            -- en DLQ par promote_retries dès le prochain sweep (30s).
            attempt_count = 0,
            last_error    = NULL
        WHERE id = ?
          AND status = 'DLQ'
        "#,
    )
    .bind(&id_str)
    .execute(pool)
    .await
    .with_context(|| format!("erreur replay DLQ job {}", id))?;

    if result.rows_affected() == 0 {
        anyhow::bail!("Job {} non trouvé en DLQ (statut ≠ DLQ ou ID inconnu)", id);
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
        format!("--older-than: format attendu '<N>d' (ex. '30d'), reçu '{spec}'")
    })?;
    let days: i64 = days_str
        .parse()
        .with_context(|| format!("--older-than: nombre de jours invalide dans '{spec}'"))?;
    if days < 0 {
        anyhow::bail!("--older-than: le nombre de jours doit être positif (reçu '{spec}')");
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
        .context("erreur comptage DLQ (prune)")?;

    let scope = match older_than {
        Some(spec) => format!("plus vieux que {spec}"),
        None => "tous".to_string(),
    };

    if targeted == 0 {
        println!("Prune DLQ ({scope}) : aucun job ciblé — rien à supprimer.");
        return Ok(());
    }

    if !apply {
        println!(
            "Prune DLQ ({scope}) — DRY-RUN : {targeted} job(s) seraient supprimés DÉFINITIVEMENT.\n\
             Relancer avec --apply pour exécuter la suppression (irréversible)."
        );
        return Ok(());
    }

    // Actual execution — permanent deletion via the store (DELETE WHERE status='DLQ').
    let deleted = store
        .delete_dlq_jobs(cutoff)
        .await
        .context("erreur suppression DLQ (prune)")?;
    println!("Prune DLQ ({scope}) : {deleted} job(s) supprimé(s) définitivement.");
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
