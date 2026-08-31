//! B11 — File de travaux : cycle de vie complet enqueue → dequeue → complete (F-145)
//!
//! Mesure le coût d'un cycle complet de job sur `SqliteQueueStore` (gradatum-db-sqlite) :
//! INSERT (enqueue) + transaction IMMEDIATE SELECT/UPDATE (dequeue) + UPDATE terminal
//! (complete), sur base SQLite **in-memory**, WAL appliqué, migrations 006→012 posées.
//!
//! C'est LE dispositif AVANT/APRÈS de la montée rusqlite (carte F-145, jalon 2.1.0) : il
//! exerce le code réel de la file (`QueueDb` → `spawn_blocking` → `blocking_lock` →
//! rusqlite), pas un binaire intermédiaire. Un seul job à la fois (aucune contention de
//! verrou tokio) : la mesure isole le coût rusqlite/SQLite — la seule variable qui change.
//!
//! Protocole reproductible :
//!   1. `open_queue_db_in_memory()` + pragmas WAL + migrations (identique aux tests).
//!   2. `iter_batched` : la table est VIDÉE hors chronométrage avant chaque itération
//!      (taille de table bornée — pas de dérive de croissance), records pré-construits.
//!   3. Une itération chronométrée = 500 cycles `enqueue → dequeue → complete`.
//!   4. Criterion : sample_size 30, médiane + CI. Rendu en ns/cycle.
//!
//! À enregistrer avec le résultat : version rusqlite, version libsqlite3-sys, version du
//! moteur SQLite embarqué (test `sqlite_engine_version` de gradatum-db-sqlite), HEAD git.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

use gradatum_core::{
    CurateSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord,
    JobResult, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, RetryBackoff,
    TriggerSource,
};
use gradatum_db_sqlite::{
    SqliteQueueStore, apply_sqlite_pragmas, open_queue_db_in_memory, run_migrations,
};
use ulid::Ulid;

/// Nombre de cycles job par itération chronométrée.
const N_CYCLES: usize = 500;

/// Construit un `JobRecord` réaliste (`Job::Curate`) — payload JSON sérialisé par enqueue.
fn make_record() -> JobRecord {
    let now = chrono::Utc::now();
    let class = JobClass::Agent;
    JobRecord {
        id: Ulid::generate(),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            class,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::default_for(&class),
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

fn bench_queue_full_cycle(c: &mut Criterion) {
    // Runtime mono-fil : mesure le coût SQLite/rusqlite pur (un seul task actif, aucune
    // contention de verrou tokio). `spawn_blocking` opère sur le pool bloquant dédié.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Setup hors boucle chaude : base in-memory, WAL, migrations 006→012.
    let store = rt.block_on(async {
        let db = open_queue_db_in_memory().await.expect("in-memory queue db");
        apply_sqlite_pragmas(&db).await.expect("pragmas WAL");
        run_migrations(&db).await.expect("migrations 006→012");
        SqliteQueueStore::new(db)
    });

    let result = JobResult {
        success: true,
        duration_ms: 150,
        cost_usd: None,
        result_note: None,
        conflict_payload: None,
    };

    let mut group = c.benchmark_group("B11-queue-full-cycle");
    group.sample_size(30);

    group.bench_function("enqueue-dequeue-complete-x500", |b| {
        b.iter_batched(
            // Setup NON chronométré : table vidée + records frais (taille bornée).
            || {
                rt.block_on(async {
                    store
                        .db()
                        .with_conn(|conn| conn.execute("DELETE FROM gradatum_jobs", []).map(|_| ()))
                        .await
                        .expect("vider gradatum_jobs");
                });
                (0..N_CYCLES).map(|_| make_record()).collect::<Vec<_>>()
            },
            // Routine chronométrée : un cycle complet par job.
            |records: Vec<JobRecord>| {
                rt.block_on(async {
                    for record in &records {
                        let id = store
                            .enqueue(black_box(record.clone()))
                            .await
                            .expect("enqueue");
                        let dequeued = store.dequeue(None).await.expect("dequeue").expect("job");
                        black_box(dequeued.id);
                        store.complete(id, result.clone()).await.expect("complete");
                    }
                });
            },
            BatchSize::NumIterations(1),
        );
    });

    group.finish();
}

criterion_group!(benches, bench_queue_full_cycle);
criterion_main!(benches);
