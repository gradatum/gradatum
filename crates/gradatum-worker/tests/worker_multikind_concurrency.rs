//! Non-régression CONCURRENCE multi-kind — `dequeue` ne deadlocke plus sous
//! charge curate+embed simultanée (bug v0.3.x « un worker draine, l'autre 0 »).
//!
//! ## Contexte (bug v0.3.x — 3 essais ratés avant le vrai fix)
//!
//! Symptôme LIVE (observé en production) : le worker Curate draine ses jobs, MAIS les jobs Embed
//! restent Pending, 0-traités pendant 5 min. Les fixes 1 (busy_timeout) et 2
//! (re-poll fetcher) ne corrigeaient qu'un symptôme (single-worker re-poll) —
//! le worker re-stallait en multi-kind.
//!
//! ## Cause racine (confirmée empiriquement par ce test)
//!
//! `SqliteQueueStore::dequeue_by_kind` est une transaction read-then-write
//! (SELECT du job Pending, puis UPDATE du lease → Running). Avec le `BEGIN`
//! DÉFÉRÉ par défaut de sqlx, le SELECT prend un verrou de lecture partagé puis
//! l'UPDATE tente de l'upgrader en verrou d'écriture exclusif. Sous charge
//! multi-worker concurrente sur le même fichier SQLite (curate conc=2 +
//! embed conc=4 cyclant rapidement begin→select→update→commit + les writes ack),
//! deux dequeues simultanés détiennent chacun un verrou de lecture et tentent
//! tous deux l'upgrade → DEADLOCK MUTUEL → `SQLITE_BUSY`. `busy_timeout` retente
//! mais sous contention soutenue une transaction est affamée indéfiniment : un
//! worker draine, l'autre reste bloqué dans son tout 1er `dequeue` (à 0).
//!
//! Le fix : `pool.begin_with("BEGIN IMMEDIATE")` — prend le verrou d'écriture
//! dès le début, plus d'upgrade read→write, les dequeues se sérialisent
//! proprement. AVANT le fix ce test fige (embed ou curate = 0/N). APRÈS : les
//! deux kinds drainent en parallèle en quelques secondes.
//!
//! ## Méthodologie
//!
//! VRAI `Monitor` Apalis, 2 workers (curate + embed) partageant le MÊME
//! `Arc<SqliteQueueStore>` (file-based WAL, comme LIVE). Backend de prod
//! (`build_gradatum_backend`), ack de prod, layers de prod, configs de prod
//! (curate conc=2, embed conc=4). Handlers STUBS (pas le LLM réel) pour isoler
//! la mécanique queue/transaction : curate ~250 ms (latence curator), embed ~5 ms.
//! Enqueue 10 Curate + 30 Embed simultanés, échantillonnage 250 ms.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use apalis::layers::WorkerBuilderExt;
use apalis::layers::retry::RetryPolicy;
use apalis::prelude::AcknowledgementExt;
use apalis::prelude::{Data, Monitor, WorkerBuilder};

use chrono::Utc;
use gradatum_core::{
    CurateSpec, EmbedSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode,
    JobOutput, JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus,
    QueueStore, RetryBackoff, TriggerSource,
};
use gradatum_db_sqlite::{SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_worker::apalis_backend::build_gradatum_backend;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Handlers stubs
// ─────────────────────────────────────────────────────────────────────────────

/// Handler curate STUB : lent (simule la latence curator LLM), compte les done.
async fn curate_stub(
    _job: GradatumJob,
    counter: Data<Arc<AtomicUsize>>,
) -> Result<JobOutput, std::convert::Infallible> {
    // Latence curator LLM réaliste (LIVE : ~300 ms/job → 13 jobs/4 s à conc=2).
    tokio::time::sleep(Duration::from_millis(250)).await;
    counter.fetch_add(1, Ordering::SeqCst);
    Ok(JobOutput::dry_run(0, "curate stub"))
}

/// Handler embed STUB : rapide (simule l'I/O embed), compte les done.
async fn embed_stub(
    _job: GradatumJob,
    counter: Data<Arc<AtomicUsize>>,
) -> Result<JobOutput, std::convert::Infallible> {
    tokio::time::sleep(Duration::from_millis(5)).await;
    counter.fetch_add(1, Ordering::SeqCst);
    Ok(JobOutput::dry_run(0, "embed stub"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Builders de records
// ─────────────────────────────────────────────────────────────────────────────

fn make_curate_record() -> JobRecord {
    let now = Utc::now();
    JobRecord {
        id: Ulid::new(),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            class: JobClass::Agent,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::default_for(&JobClass::Agent),
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry {
            count: 0,
            max: 3,
            backoff: RetryBackoff::Exponential { base: 5, max: 120 },
            last_error: None,
            errors: vec![],
        },
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

fn make_embed_record() -> JobRecord {
    let now = Utc::now();
    JobRecord {
        id: Ulid::new(),
        spec: JobSpec {
            kind: Job::Embed(EmbedSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                force_regenerate: false,
            }),
            class: JobClass::Agent,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::Normal,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry {
            count: 0,
            max: 3,
            backoff: RetryBackoff::Exponential { base: 5, max: 120 },
            last_error: None,
            errors: vec![],
        },
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test repro multi-kind
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn embed_does_not_stall_under_curate_load() {
    use tokio::runtime::Builder;

    // Runtime dédié au Monitor — réplique LIVE `#[tokio::main(flavor="multi_thread")]`
    // SANS worker_threads explicite. Sur un hôte 4 cœurs le runtime LIVE a donc
    // ~4 worker threads. On fixe 4 ici pour reproduire la même topologie.
    let monitor_rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("monitor runtime");
    let orch_rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("orchestration runtime");

    let (store, curate_done, embed_done, curate_backend, curate_ack, embed_backend, embed_ack) =
        orch_rt.block_on(async {
            let tmp = tempfile::NamedTempFile::new().expect("tmpfile");
            let path = tmp.path().to_path_buf();
            std::mem::forget(tmp);
            let opts = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_secs(5));
            let pool = SqlitePoolOptions::new()
                .max_connections(8)
                .connect_with(opts)
                .await
                .expect("pool");
            apply_sqlite_pragmas(&pool).await.expect("pragmas");
            run_migrations(&pool).await.expect("migrations");

            let store: Arc<dyn QueueStore + Send + Sync> =
                Arc::new(SqliteQueueStore::new(pool.clone()));
            let curate_done = Arc::new(AtomicUsize::new(0));
            let embed_done = Arc::new(AtomicUsize::new(0));

            let (cb, ca) =
                build_gradatum_backend(Arc::clone(&store), "Curate").expect("curate backend");
            let (eb, ea) =
                build_gradatum_backend(Arc::clone(&store), "Embed").expect("embed backend");
            (store, curate_done, embed_done, cb, ca, eb, ea)
        });

    // Monitor = curate (conc=2) + embed (conc=4), layers de prod. Réplique monitor.rs.
    let curate_done_w = Arc::clone(&curate_done);
    let embed_done_w = Arc::clone(&embed_done);
    let monitor = Monitor::new()
        .register({
            let backend = curate_backend;
            let ack = curate_ack;
            let cnt = curate_done_w;
            move |idx| {
                let name = format!("curate-{idx}");
                WorkerBuilder::new(&name)
                    .backend(backend.clone())
                    .data(Arc::clone(&cnt))
                    .ack_with(ack.clone())
                    .enable_tracing()
                    .timeout(Duration::from_secs(30))
                    .retry(RetryPolicy::retries(3))
                    .catch_panic()
                    .concurrency(2)
                    .build(curate_stub)
            }
        })
        .register({
            let backend = embed_backend;
            let ack = embed_ack;
            let cnt = embed_done_w;
            move |idx| {
                let name = format!("embed-{idx}");
                WorkerBuilder::new(&name)
                    .backend(backend.clone())
                    .data(Arc::clone(&cnt))
                    .ack_with(ack.clone())
                    .enable_tracing()
                    .timeout(Duration::from_secs(60))
                    .retry(RetryPolicy::retries(3))
                    .catch_panic()
                    .concurrency(4)
                    .build(embed_stub)
            }
        });
    let monitor_handle = monitor_rt.spawn(async move { monitor.run().await });

    let result = orch_rt.block_on(async {
        // Enqueue MIX simultané : 10 Curate + 30 Embed.
        for _ in 0..10 {
            store
                .enqueue(make_curate_record())
                .await
                .expect("enqueue curate");
        }
        for _ in 0..30 {
            store
                .enqueue(make_embed_record())
                .await
                .expect("enqueue embed");
        }

        // Échantillonnage 250 ms × 120 = 30 s. On note le 1er instant où embed
        // progresse (embed_done > 0) ET combien de curate restent à faire à ce moment.
        let mut first_embed_at_sample: Option<usize> = None;
        let mut curate_at_first_embed: usize = 0;
        let mut samples: Vec<(usize, usize)> = Vec::new();
        for i in 0..120 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let c = curate_done.load(Ordering::SeqCst);
            let e = embed_done.load(Ordering::SeqCst);
            samples.push((c, e));
            if first_embed_at_sample.is_none() && e > 0 {
                first_embed_at_sample = Some(i);
                curate_at_first_embed = c;
            }
            if c >= 10 && e >= 30 {
                break;
            }
        }
        (
            curate_done.load(Ordering::SeqCst),
            embed_done.load(Ordering::SeqCst),
            first_embed_at_sample,
            curate_at_first_embed,
            samples,
        )
    });

    monitor_handle.abort();

    let (c_final, e_final, first_embed, curate_at_first_embed, samples) = result;

    // Trace lisible de la progression (1 ligne / 1 s).
    eprintln!("=== progression (curate_done, embed_done) toutes les 1 s ===");
    for (i, (c, e)) in samples.iter().enumerate() {
        if i % 4 == 0 {
            eprintln!("t={:>4}ms  curate={:>2}  embed={:>2}", (i + 1) * 250, c, e);
        }
    }
    eprintln!(
        "final: curate={c_final}/10  embed={e_final}/30  \
         first_embed_progress_at_sample={first_embed:?}  curate_done_at_that_moment={curate_at_first_embed}"
    );

    // Non-régression : les DEUX kinds doivent drainer entièrement (pas de deadlock).
    assert!(
        c_final >= 10,
        "DEADLOCK dequeue : curate={c_final}/10 non drainés (embed={e_final}/30). \
         Régression du fix BEGIN IMMEDIATE (upgrade read→write deadlock)."
    );
    assert!(
        e_final >= 30,
        "DEADLOCK dequeue : embed={e_final}/30 non drainés (curate={c_final}/10). \
         Régression du fix BEGIN IMMEDIATE (upgrade read→write deadlock)."
    );
    // Les Embed ne doivent PAS attendre la fin du drain Curate : ils progressent
    // en parallèle (embed=5 ms ≪ curate=250 ms → embed avance bien avant que
    // curate ait fini ses 10 jobs lents).
    assert!(
        curate_at_first_embed < 9,
        "STARVATION : embed n'a progressé qu'APRÈS drain quasi-complet de curate \
         (curate_done={curate_at_first_embed} au 1er embed). Sous BEGIN IMMEDIATE \
         les dequeues se sérialisent équitablement — embed ne doit pas attendre curate."
    );
}
