//! The periodic purge of terminal payloads, measured on the file after **one scheduler tick**.
//!
//! The store-level tests measure `purge_terminal_payloads` through the store API. This one
//! measures something else: that the periodic task the worker actually spawns erases the
//! bodies, on a real `queue.sqlite`, and that it erases *only* them. `run_queue_purge_once`
//! is the exact body of the spawned loop — one call here is one tick there, so a green run
//! is a statement about the scheduler and not only about the store.
//!
//! Protocol: a `TempDir` queue populated through the real `enqueue` path with
//!
//! - `PENDING` non-terminal jobs, oldest ULIDs of all — the strongest position for them: the
//!   candidate predicate must exclude them by *status*, never by accident of ordering;
//! - enough old terminal jobs to overflow the keep window, each carrying a **unique** witness
//!   in its note body (`CurateSpec.body`), `created_at` well past the retention cutoff;
//! - a few recent terminal jobs, inside the cutoff.
//!
//! One tick. The purge is then read three ways: the count the tick returns, the dry-run count
//! the admin path uses (before and after → the two agree, and the second one is `0`), and the
//! file itself — raw bytes, then the `payload` column after re-opening. The control that keeps
//! the absence honest: every witness that must survive (kept window, recent, non-terminal) is
//! asserted **present** in the same bytes. A shared witness would prove nothing — the kept
//! rows would still carry it and the check would be green by construction, which is why each
//! job carries its own.

use std::path::Path;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use gradatum_core::job::{CurateSpec, TERMINAL_PAYLOAD_KEEP};
use gradatum_core::{
    Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TriggerSource,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use gradatum_worker::WorkerMetrics;
use gradatum_worker::schedules::{QueueConfig, run_queue_purge_once};
use tempfile::TempDir;
use ulid::Ulid;

/// Retention of the config under test — the compiled default, spelled out.
const PURGE_AFTER_SECS: u64 = 86_400;
/// Non-terminal jobs: they must survive the purge untouched.
const PENDING: u64 = 4;
/// Old terminal jobs. Chosen so that `TERMINAL + RECENT` overflows the keep window and the
/// purge has something to do: the assertion below derives the expected count from
/// `TERMINAL_PAYLOAD_KEEP`, so this constant is not a second copy of the window.
const TERMINAL_OLD: u64 = 215;
/// Recent terminal jobs: terminal, but younger than the cutoff.
const TERMINAL_RECENT: u64 = 5;

/// ULID of the job at `seq`, explicitly ordered.
///
/// The seq ranges are disjoint on purpose: non-terminal jobs take the **lowest** ULIDs (so a
/// missing status filter would eat them), old terminal jobs the middle band, recent terminal
/// jobs the highest (so the keep window — the N most recent terminal rows — is made of them
/// plus the top of the middle band).
fn job_id(seq: u64) -> Ulid {
    Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq) + 1)
}

/// Unique witness of the job at `seq` — the string whose absence is the proof sought.
fn witness(seq: u64) -> String {
    format!("WITNESS-QUEUE-PURGE-{seq:04}")
}

/// A job at `seq` carrying `witness(seq)` in its note body, `age` old, in `status`.
fn record(seq: u64, status: JobStatus, age: ChronoDuration) -> JobRecord {
    let created_at = Utc::now() - age;
    JobRecord {
        id: job_id(seq),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                body: Some(witness(seq)),
                ..Default::default()
            }),
            class: JobClass::Agent,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::default_for(&JobClass::Agent),
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: created_at,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status,
            created_at,
            started_at: None,
            completed_at: (status != JobStatus::Pending).then_some(created_at),
            lease_until: None,
            result: None,
        },
        retry: JobRetry::default(),
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

/// Reads **every** file of the database (`queue.sqlite` and its `-wal` if it survives) and
/// returns, per file, the decoded text.
fn read_db_files(dir: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read_dir TempDir") {
        let entry = entry.expect("TempDir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("queue.sqlite") {
            let bytes = std::fs::read(entry.path()).expect("re-read a database file");
            files.push((name, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    files
}

/// Current value of the purge counter, summed over its children.
fn purged_metric(metrics: &WorkerMetrics) -> u64 {
    metrics
        .registry()
        .gather()
        .iter()
        .filter(|family| family.name() == "gradatum_queue_payloads_purged_total")
        .flat_map(|family| family.get_metric())
        .map(|metric| metric.get_counter().get_value() as u64)
        .sum()
}

#[tokio::test]
async fn one_scheduler_tick_erases_only_the_old_terminal_payloads() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("queue.sqlite");

    let retained_total = PENDING + TERMINAL_OLD + TERMINAL_RECENT;
    let terminal_total = TERMINAL_OLD + TERMINAL_RECENT;
    let expected_purged = terminal_total - TERMINAL_PAYLOAD_KEEP;

    // The tick must be non-trivial: a queue that fits entirely in the keep window would make
    // every absence below green while proving nothing about the purge.
    assert!(
        expected_purged > 0 && expected_purged < TERMINAL_OLD,
        "fixture degenerate: {terminal_total} terminal rows against a keep window of \
         {TERMINAL_PAYLOAD_KEEP} leaves {expected_purged} to purge — the test would not \
         distinguish a working purge from a missing one"
    );

    let metrics = WorkerMetrics::new();
    let cutoff = QueueConfig {
        purge_after_secs: PURGE_AFTER_SECS,
        purge_interval_secs: 3_600,
    }
    .purge_cutoff();

    {
        let pool = QueueDb::open(&path).await.expect("open queue.sqlite");
        run_migrations(&pool).await.expect("migrations");
        let store = SqliteQueueStore::new(pool);

        for seq in 0..PENDING {
            store
                .enqueue(record(seq, JobStatus::Pending, ChronoDuration::hours(48)))
                .await
                .expect("enqueue a non-terminal job");
        }
        for seq in 100..(100 + TERMINAL_OLD) {
            store
                .enqueue(record(seq, JobStatus::Done, ChronoDuration::hours(48)))
                .await
                .expect("enqueue an old terminal job");
        }
        for seq in 1000..(1000 + TERMINAL_RECENT) {
            store
                .enqueue(record(seq, JobStatus::Done, ChronoDuration::zero()))
                .await
                .expect("enqueue a recent terminal job");
        }

        // The dry-run count and the purge must designate the same rows — the count feeds the
        // admin `--dry-run`, so a divergence here is an operator told the wrong number.
        assert_eq!(
            store
                .count_purgeable_terminal_payloads(Some(cutoff))
                .await
                .expect("count before the tick"),
            expected_purged,
            "the dry-run count must designate exactly what the purge erases"
        );

        // ── One tick of the scheduler ────────────────────────────────────────────────
        let purged = run_queue_purge_once(&store, &QueueConfig::default(), &metrics).await;
        assert_eq!(
            purged, expected_purged,
            "one tick erases the {expected_purged} old terminal payloads outside the keep \
             window — and nothing else"
        );
        assert_eq!(
            purged_metric(&metrics),
            expected_purged,
            "the tick reports the same number to the metric as it returns"
        );

        // Idempotence: the rows are rewritten, not deleted, so a second tick has nothing left
        // to do — and the survivors must not be candidates again.
        assert_eq!(
            run_queue_purge_once(&store, &QueueConfig::default(), &metrics).await,
            0,
            "a second tick on the same queue is a no-op"
        );
        assert_eq!(
            store
                .count_purgeable_terminal_payloads(Some(cutoff))
                .await
                .expect("count after the tick"),
            0,
            "after the tick the dry-run count is empty — the two paths still agree"
        );
        assert_eq!(
            purged_metric(&metrics),
            expected_purged,
            "a tick with nothing to do adds nothing to the metric"
        );
    } // store and `QueueDb` handle dropped → connection closed, WAL checkpointed

    // ── Raw bytes, every file of the database ────────────────────────────────────────
    let files = read_db_files(dir.path());
    assert!(
        !files.is_empty(),
        "the database files must be there to be read"
    );
    let whole: String = files.iter().map(|(_, text)| text.as_str()).collect();

    // The control first: what must survive has to be present, or the absences below would be
    // green on an empty, unreadable, or simply wrong file.
    for seq in 0..PENDING {
        assert!(
            whole.contains(&witness(seq)),
            "witness {} (non-terminal) must survive the purge — non-terminal jobs are \
             outside its remit",
            witness(seq)
        );
    }
    for seq in (100 + expected_purged)..(100 + TERMINAL_OLD) {
        assert!(
            whole.contains(&witness(seq)),
            "witness {} (inside the keep window) must survive the purge",
            witness(seq)
        );
    }
    for seq in 1000..(1000 + TERMINAL_RECENT) {
        assert!(
            whole.contains(&witness(seq)),
            "witness {} (recent terminal) must survive the purge — it is younger than the \
             retention cutoff",
            witness(seq)
        );
    }

    for (name, text) in &files {
        let residual: Vec<u64> = (100..(100 + expected_purged))
            .filter(|seq| text.contains(&witness(*seq)))
            .collect();
        assert!(
            residual.is_empty(),
            "purge tick — {name}: {}/{} purged witnesses still readable in the raw bytes \
             (rows {:?}); expected 0: the connection opens with secure_delete=ON and the \
             purge ends with a truncating checkpoint",
            residual.len(),
            expected_purged,
            residual,
        );
    }

    // ── The `payload` column, file re-opened ─────────────────────────────────────────
    let pool = QueueDb::open_existing(&path)
        .await
        .expect("re-open queue.sqlite");
    let rows: Vec<(String, String, String)> = pool
        .with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, status, payload FROM gradatum_jobs ORDER BY id")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect()
        })
        .await
        .expect("read the payloads back from the re-opened file");

    assert_eq!(
        rows.len() as u64,
        retained_total,
        "the purge rewrites rows, it deletes none — every job is still there"
    );

    let mut checked_purged = 0u64;
    for (id, status, payload) in &rows {
        let seq = (0..PENDING)
            .chain(100..(100 + TERMINAL_OLD))
            .chain(1000..(1000 + TERMINAL_RECENT))
            .find(|seq| job_id(*seq).to_string() == *id)
            .unwrap_or_else(|| panic!("unknown row id {id} — not produced by this fixture"));
        let w = witness(seq);
        let outside_window = (100..(100 + expected_purged)).contains(&seq);
        if outside_window {
            checked_purged += 1;
            assert_eq!(
                status, "Done",
                "row {seq}: the purge leaves the lifecycle alone"
            );
            assert!(
                !payload.contains(&w),
                "row {seq}: a purged payload must no longer carry its note body"
            );
            assert!(
                payload.contains("PayloadPurged"),
                "row {seq}: the purge carries the sentinel in place of `spec.kind`"
            );
        } else {
            assert!(
                payload.contains(&w),
                "row {seq} (status {status}): an out-of-window payload must be untouched"
            );
        }
    }
    assert_eq!(
        checked_purged, expected_purged,
        "every purged row was found again in the re-opened file — the witness list and the \
         database agree"
    );

    let store = SqliteQueueStore::new(pool);
    for seq in 0..PENDING {
        let row = store
            .get(job_id(seq), None)
            .await
            .expect("get a non-terminal job")
            .expect("a non-terminal job survives the purge");
        assert_eq!(
            row.lifecycle.status,
            JobStatus::Pending,
            "the purge does not promote, cancel or otherwise move a non-terminal job"
        );
        assert!(
            matches!(&row.spec.kind, Job::Curate(spec) if spec.body.as_deref() == Some(witness(seq).as_str())),
            "the non-terminal job keeps the note body it was enqueued with"
        );
    }
}

/// The two keys of the schedule itself: the compiled defaults, and the floor under the tick.
///
/// The purge erases data; a `0` interval would turn the loop into a spin and a `0` retention
/// would designate the whole queue. Both are refused rather than clamped silently — the
/// deserializer rejects the first, and this test pins the second.
#[test]
fn schedule_defaults_are_the_documented_ones() {
    let cfg = QueueConfig::default();
    assert_eq!(
        cfg.purge_after_secs, PURGE_AFTER_SECS,
        "compiled default retention: 24 h"
    );
    assert_eq!(
        cfg.purge_interval(),
        Duration::from_secs(3_600),
        "compiled default tick: 1 h"
    );

    let cut = cfg.purge_cutoff();
    let expected = Utc::now() - ChronoDuration::seconds(PURGE_AFTER_SECS as i64);
    assert!(
        (cut - expected).num_seconds().abs() < 5,
        "the cutoff is `now - purge_after_secs` (got {cut}, expected about {expected})"
    );

    // The `load_section` fallback path builds `T::default()` — a derived `Default` would put
    // `0` here and the cutoff at `now`, designating the whole queue the first time the section
    // is missing or malformed.
    assert_eq!(
        QueueConfig {
            purge_after_secs: 0,
            purge_interval_secs: 0
        }
        .purge_interval(),
        Duration::from_secs(60),
        "a zero interval is floored at 60 s rather than spinning"
    );
}
