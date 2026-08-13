//! Non-régression worker-hang — le worker re-poll après avoir drainé un batch.
//!
//! ## Contexte (bug v0.3.x)
//!
//! Le `GradatumBackend` custom (Apalis) tirait ses jobs via un `stream::unfold`
//! dont le fetcher contenait un `loop {}` interne : sur file vide il dormait
//! 200ms et rebouclait SANS rendre la main au worker (ne yieldait que sur
//! `Some(record)`). Conséquence empirique : le worker traitait un micro-batch
//! (0-2 jobs selon le timing) puis se figeait — les jobs `Pending` restants
//! n'étaient jamais dequeués, même après expiration des leases ; un restart
//! systemd relançait un micro-batch puis re-figeait (cyclique).
//!
//! ## Cause racine
//!
//! Le driver Apalis `CallAll::poll_next` gate le poll du stream de jobs derrière
//! le `poll_ready` du limiter `concurrency` (tower `ConcurrencyLimitLayer`).
//! Avec un fetcher qui ne rend jamais la main sur file vide, le réveil du timer
//! `sleep` et celui de la libération d'un permit de concurrence se courent — un
//! wakeup est perdu, le stream n'est plus re-pollé, le worker s'arrête
//! (« Worker curate-0 stopped » puis silence).
//!
//! Le fix : pattern fetcher canonique Apalis — UN poll = UN dequeue ; sur file
//! vide yield `Ok(None)` (event Idle) et rendre la main. Le worker re-drive
//! alors le stream lui-même, aucun wakeup n'est perdu.
//!
//! ## Ce test
//!
//! Démarre un VRAI `Monitor` Apalis avec le backend de production
//! (`build_gradatum_backend`) et les layers faithful de prod (ack_with,
//! enable_tracing, timeout, retry, catch_panic, concurrency=2), handler no-op
//! qui compte. Enqueue 5 jobs, drain, enqueue 5 NOUVEAUX, vérifie le re-traitement.
//!
//! AVANT le fix : l'étape 1 elle-même stagnait. APRÈS : drain complet + re-poll.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use apalis::layers::WorkerBuilderExt;
use apalis::layers::retry::RetryPolicy;
use apalis::prelude::AcknowledgementExt;
use apalis::prelude::{Data, Monitor, WorkerBuilder};

use chrono::Utc;
use gradatum_core::{
    CurateSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobOutput,
    JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, QueueStore,
    RetryBackoff, TriggerSource,
};
use gradatum_db_sqlite::{SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_worker::apalis_backend::build_gradatum_backend;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use ulid::Ulid;

/// Handler no-op : compte les jobs traités. Jamais d'échec (`Infallible`).
async fn noop_handler(
    _job: GradatumJob,
    counter: Data<Arc<AtomicUsize>>,
) -> Result<JobOutput, std::convert::Infallible> {
    counter.fetch_add(1, Ordering::SeqCst);
    Ok(JobOutput::dry_run(0, "noop repoll"))
}

/// Construit un `JobRecord` Curate Pending minimal.
fn make_curate_record() -> JobRecord {
    let now = Utc::now();
    JobRecord {
        id: Ulid::generate(),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
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

/// Enqueue `n` jobs Curate Pending.
async fn enqueue_n(store: &Arc<dyn QueueStore + Send + Sync>, n: usize) {
    for _ in 0..n {
        store.enqueue(make_curate_record()).await.expect("enqueue");
    }
}

/// Attend que le compteur atteigne `target` ; ceiling très généreux (120s) pour
/// absorber la contention CI (compilation parallèle + nombreux process de test).
/// Le test retourne dès que la cible est atteinte — le ceiling n'est consommé
/// qu'en cas d'échec réel (régression du re-poll).
async fn wait_count(counter: &Arc<AtomicUsize>, target: usize) -> bool {
    for _ in 0..1200 {
        if counter.load(Ordering::SeqCst) >= target {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Test `#[test]` (pas `#[tokio::test]`) — ISOLATION runtime.
///
/// Le `Monitor` Apalis tourne sur son PROPRE runtime multi-thread dédié, dans un
/// thread OS séparé. L'orchestration (enqueue / wait) tourne sur un petit runtime
/// distinct. Sans cette isolation, le polling du Monitor et le `wait_count`
/// partagent les mêmes threads tokio ; sous la contention de `cargo test`
/// (compilation parallèle + dizaines de process de test), ces threads sont
/// affamés et le test devient flaky (faux négatif de re-poll). Le runtime dédié
/// garantit au Monitor du CPU quel que soit l'environnement de test.
#[test]
fn worker_repolls_after_drain() {
    use tokio::runtime::Builder;

    // Runtime dédié au Monitor (4 threads garantis).
    let monitor_rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("monitor runtime");
    // Runtime d'orchestration (setup + enqueue + wait).
    let orch_rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("orchestration runtime");

    // Setup pool + store sur le runtime d'orchestration.
    let (store, counter, backend, ack) = orch_rt.block_on(async {
        // Pool SQLite fichier WAL (réaliste vs LIVE — pas :memory: multi-conn piégeux).
        let tmp = tempfile::NamedTempFile::new().expect("tmpfile");
        // Conserver le tmpfile vivant en le leakant — le test est court et le fichier
        // est nettoyé par l'OS ; évite que le drop ferme le fichier sous le pool.
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let opts = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .expect("pool");
        apply_sqlite_pragmas(&pool).await.expect("pragmas");
        run_migrations(&pool).await.expect("migrations");

        let store: Arc<dyn QueueStore + Send + Sync> =
            Arc::new(SqliteQueueStore::new(pool.clone()));
        let counter = Arc::new(AtomicUsize::new(0));
        let (backend, ack) = build_gradatum_backend(Arc::clone(&store), "Curate").expect("backend");
        (store, counter, backend, ack)
    });

    // Démarrer le Monitor sur son runtime dédié.
    let counter_w = Arc::clone(&counter);
    let monitor = Monitor::new().register(move |idx| {
        let name = format!("curate-{idx}");
        WorkerBuilder::new(&name)
            .backend(backend.clone())
            .data(Arc::clone(&counter_w))
            .ack_with(ack.clone())
            .enable_tracing()
            .timeout(Duration::from_secs(30))
            .retry(RetryPolicy::retries(3))
            .catch_panic()
            .concurrency(2)
            .build(noop_handler)
    });
    let monitor_handle = monitor_rt.spawn(async move { monitor.run().await });

    // Orchestration : enqueue + wait sur le runtime d'orchestration.
    orch_rt.block_on(async {
        // ── Étape 1 : batch initial ────────────────────────────────────────────
        enqueue_n(&store, 5).await;
        let batch1_ok = wait_count(&counter, 5).await;
        assert!(
            batch1_ok,
            "batch initial non drainé : compteur = {} (attendu >= 5) — \
             régression worker-hang (le fetcher ne re-poll plus après micro-batch)",
            counter.load(Ordering::SeqCst)
        );

        // Laisser le worker re-poller une file vide (drain complet).
        tokio::time::sleep(Duration::from_secs(1)).await;

        // ── Étape 2 : ré-enqueue après drain ───────────────────────────────────
        enqueue_n(&store, 5).await;
        let batch2_ok = wait_count(&counter, 10).await;

        assert!(
            batch2_ok,
            "jobs ré-enqueués après drain NON traités — compteur = {} (attendu >= 10). \
             Le worker ne re-poll plus après avoir vidé le batch initial.",
            counter.load(Ordering::SeqCst)
        );
    });

    monitor_handle.abort();
}
