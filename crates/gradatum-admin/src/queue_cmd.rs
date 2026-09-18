//! `gradatum-admin queue` — operator surface over the queue's maintenance.
//!
//! One sub-command, `purge`, and it exists because the queue's payload purge used to be
//! reachable from no operator path at all: the store implemented it, the worker scheduled
//! it, and nobody could run it once, on demand, to see it work or to reclaim the space
//! before the next tick.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use gradatum_core::{QueueStore, TERMINAL_PAYLOAD_KEEP};
use gradatum_db_sqlite::SqliteQueueStore;

use crate::jobs_cmd::{open_queue_pool, parse_older_than};

/// Retention window applied when `--older-than` is absent, in the parser's own syntax.
///
/// It restates the worker's compiled `queue.purge_after_secs` default (24 h) rather than
/// reading it: `gradatum-admin` does not link the worker. The two are kept in step by the
/// documentation of that key, which names the CLI default — an operator who changes the
/// configuration and expects the CLI to follow it would otherwise be surprised in the
/// direction that erases data.
const DEFAULT_RETENTION: &str = "24h";

/// Sub-commands of `queue`.
#[derive(Debug, Subcommand)]
pub enum QueueCmd {
    /// Erases the note body of terminal jobs outside the retention window.
    Purge(QueuePurgeArgs),
}

/// Arguments for `queue purge`.
#[derive(Debug, Args)]
pub struct QueuePurgeArgs {
    /// Gradatum root directory.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// Designates the targeted payloads without writing anything.
    ///
    /// Prints the exact count an execution would erase — the same predicate, counted
    /// instead of applied — and leaves every row untouched.
    #[arg(long)]
    pub dry_run: bool,

    /// Retention window: `<N>d` or `<N>h` (e.g. `30d`, `12h`).
    ///
    /// Jobs that reached a terminal status **before** `now - window` lose their payload.
    /// Defaults to 24 h, the retention the worker applies on its own.
    #[arg(long, value_name = "duree")]
    pub older_than: Option<String>,
}

/// Main dispatch for `gradatum-admin queue`.
pub async fn run(cmd: QueueCmd) -> Result<()> {
    match cmd {
        QueueCmd::Purge(args) => queue_purge(args).await,
    }
}

/// `queue purge` — erases the note body of terminal jobs past the retention window.
///
/// # Why this one acts by default
///
/// `jobs dlq --prune` is dry-run by default, and this command is not. The difference is in
/// what each destroys: pruning a DLQ deletes rows and the record of what happened, while a
/// payload purge rewrites the body of rows that stay in place — the job, its status, its
/// timestamps and its result note are all still there afterwards, and the operation is the
/// very one the worker already performs every `queue.purge_interval` (1 h by default). An
/// operator running it by hand is asking for that same maintenance now, not discovering a
/// new power; `--dry-run` is there for the question that precedes it.
///
/// Nothing here decides *which* rows are eligible. The window is the store's — the same
/// predicate as the scheduled purge and as `--dry-run`'s count — so the three surfaces
/// cannot drift apart.
///
/// # A second writer on `queue.sqlite`
///
/// The operator path opens its own write connection on the queue file, so running it while the
/// worker is up puts two writers on one database — safe (WAL, a 5 s `busy_timeout`, and a purge
/// that is idempotent: a row already carrying `PayloadPurged` is neither rewritten nor counted),
/// but open to contention with the scheduled purge, which comes back as a transient
/// `SQLITE_BUSY` to run again rather than a failure to investigate.
async fn queue_purge(args: QueuePurgeArgs) -> Result<()> {
    let spec = args.older_than.as_deref().unwrap_or(DEFAULT_RETENTION);
    let cutoff = parse_older_than(spec)?;
    let scope = match args.older_than {
        Some(_) => format!("--older-than {spec}"),
        None => format!("default retention {DEFAULT_RETENTION}"),
    };

    let pool = open_queue_pool(&args.root).await?;
    let store = SqliteQueueStore::new(pool);

    // The keep window is applied by the store, not here — but it is the reason a recent
    // terminal job survives a cutoff that has passed it, and an operator reading a count
    // that looks too low needs the reason next to it.
    let window = format!(
        "the {TERMINAL_PAYLOAD_KEEP} most recent terminal rows are never touched, whatever the cutoff"
    );

    if args.dry_run {
        // Same predicate, counted instead of applied.
        let targeted = store
            .count_purgeable_terminal_payloads(Some(cutoff))
            .await
            .context("queue payload count error (dry-run)")?;
        println!(
            "Dry-run ({scope}): {targeted} payload(s) would be erased — cutoff {cutoff}; \
             {window}. Nothing was written."
        );
        return Ok(());
    }

    let purged = store
        .purge_terminal_payloads(Some(cutoff))
        .await
        .context("queue payload purge error")?;
    println!(
        "Queue purge ({scope}): {purged} payload(s) erased — cutoff {cutoff}; {window}. \
         The rows themselves are kept: only their note body is gone."
    );
    Ok(())
}
