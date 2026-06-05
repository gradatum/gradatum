//! `gradatum-admin jobs {list,get,cancel,dlq}` — introspection de la queue F-16.
//!
//! ## Sous-commandes
//!
//! ```text
//! gradatum-admin jobs list   --root /var/lib/gradatum [--status pending] [--kind Curate] [--limit 50]
//! gradatum-admin jobs get    --root /var/lib/gradatum <id>
//! gradatum-admin jobs cancel --root /var/lib/gradatum <id>
//! gradatum-admin jobs dlq    --root /var/lib/gradatum [--replay <id>] [--replay-all]
//! ```
//!
//! ## Accès direct SQLite
//!
//! Les commandes accèdent directement à la base SQLite (WAL) sans passer par le serveur HTTP.
//! Le chemin de la base est dérivé de `--root` : `<root>/db/queue.sqlite`.
//!
//! ## Références
//!
//! - spec §6 Phase 3 tâche 5 — CLI admin jobs
//! - v81 F-16 §6 L5613-5668

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use gradatum_core::{JobFilter, JobRecord, JobStatus, QueueStore};
use gradatum_db_sqlite::{apply_sqlite_pragmas, run_migrations, SqliteQueueStore};
use sqlx::SqlitePool;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Sous-commandes
// ─────────────────────────────────────────────────────────────────────────────

/// Sous-commandes de `jobs`.
#[derive(Debug, Subcommand)]
pub enum JobsCmd {
    /// Liste les jobs avec filtres optionnels.
    List(JobsListArgs),
    /// Affiche le détail d'un job par son ULID.
    Get(JobsGetArgs),
    /// Annule un job (409 si Running).
    Cancel(JobsCancelArgs),
    /// Opérations Dead Letter Queue — liste et replay.
    Dlq(JobsDlqArgs),
}

/// Arguments de `jobs list`.
#[derive(Debug, Args)]
pub struct JobsListArgs {
    /// Répertoire racine Gradatum.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// Filtre par statut (pending, running, done, failed, dlq, cancelled).
    #[arg(long)]
    pub status: Option<String>,

    /// Filtre par kind (Curate, Embed, Summarize, …).
    #[arg(long)]
    pub kind: Option<String>,

    /// Nombre max de résultats (défaut 50, max 200).
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

/// Arguments de `jobs get`.
#[derive(Debug, Args)]
pub struct JobsGetArgs {
    /// Répertoire racine Gradatum.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// ULID du job.
    pub id: String,
}

/// Arguments de `jobs cancel`.
#[derive(Debug, Args)]
pub struct JobsCancelArgs {
    /// Répertoire racine Gradatum.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// ULID du job à annuler.
    pub id: String,
}

/// Arguments de `jobs dlq`.
#[derive(Debug, Args)]
pub struct JobsDlqArgs {
    /// Répertoire racine Gradatum.
    #[arg(long, default_value = "/var/lib/gradatum")]
    pub root: PathBuf,

    /// Replay un job DLQ individuel (ULID) — remet en Pending.
    #[arg(long)]
    pub replay: Option<String>,

    /// Replay tous les jobs DLQ — remet tous en Pending.
    #[arg(long)]
    pub replay_all: bool,

    /// Nombre max de jobs à lister (défaut 50).
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Entrée principale
// ─────────────────────────────────────────────────────────────────────────────

/// Dispatch principal de `gradatum-admin jobs`.
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

/// Ouvre le pool SQLite de la queue depuis `<root>/db/queue.sqlite`.
///
/// Applique les pragmas WAL et exécute les migrations.
async fn open_queue_pool(root: &std::path::Path) -> Result<SqlitePool> {
    let db_path = root.join("db").join("queue.sqlite");
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

/// Formate un `JobRecord` en une ligne résumée pour `list`.
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

/// Parse une chaîne en `JobStatus` (insensible à la casse).
fn parse_status(s: &str) -> Result<JobStatus> {
    match s.to_lowercase().as_str() {
        "pending" => Ok(JobStatus::Pending),
        "running" => Ok(JobStatus::Running),
        "waiting" => Ok(JobStatus::Waiting),
        "done" => Ok(JobStatus::Done),
        "failed" => Ok(JobStatus::Failed),
        "dlq" => Ok(JobStatus::DLQ),
        "cancelled" | "canceled" => Ok(JobStatus::Cancelled),
        other => anyhow::bail!("statut inconnu : '{}' (valeurs valides : pending, running, waiting, done, failed, dlq, cancelled)", other),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────────────

/// `jobs list` — liste les jobs avec filtres optionnels.
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

/// `jobs get` — affiche le détail d'un job.
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
            // JSON pretty-print du JobRecord complet
            let json = serde_json::to_string_pretty(&record)
                .context("erreur sérialisation JSON JobRecord")?;
            println!("{}", json);
        }
    }

    Ok(())
}

/// `jobs cancel` — annule un job (409 si Running).
async fn jobs_cancel(args: JobsCancelArgs) -> Result<()> {
    let id = args
        .id
        .parse::<Ulid>()
        .with_context(|| format!("ULID invalide : '{}'", args.id))?;

    let pool = open_queue_pool(&args.root).await?;
    let store = SqliteQueueStore::new(pool);

    // Vérifie l'état courant avant annulation
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

/// `jobs dlq` — liste les jobs DLQ et replay optionnel.
async fn jobs_dlq(args: JobsDlqArgs) -> Result<()> {
    let pool = open_queue_pool(&args.root).await?;
    let store = SqliteQueueStore::new(pool.clone());

    // Replay individuel
    if let Some(ref id_str) = args.replay {
        let id = id_str
            .parse::<Ulid>()
            .with_context(|| format!("ULID invalide : '{}'", id_str))?;
        return replay_single(&store, &pool, id).await;
    }

    // Liste les jobs DLQ
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

    // Replay tous si demandé
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

/// Replay d'un job DLQ individuel : remet en Pending via mise à jour directe.
///
/// Note : v0.2.0 Bronze — `set_pending()` est planifié F-14 v0.3.0 (cascade DAG).
/// Ici on utilise une requête SQL directe pour le replay DLQ (hors chaînage DAG).
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
