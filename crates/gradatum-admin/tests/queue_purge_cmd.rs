//! The `gradatum-admin queue purge` command, measured on the binary that ships.
//!
//! The command is a thin wrapper over the store's purge, so the interesting claims are not
//! about the purge itself — the store tests cover that — but about what the operator gets:
//! that `--dry-run` designates exactly the rows an execution erases (and erases none), that
//! `--older-than` really narrows the cutoff instead of being printed and ignored, and that an
//! execution is idempotent. Those live in the command, so they are measured by running the
//! command: `CARGO_BIN_EXE_gradatum-admin`, its exit code, and what it did to a file-backed
//! queue in a `TempDir`.
//!
//! Same fixture shape as the worker's scheduler test, so the two agree by construction:
//! non-terminal jobs take the lowest ULIDs, terminal jobs old enough to leave the keep window
//! carry unique witnesses, and the most recent terminal rows sit inside it.

use std::path::Path;
use std::process::Command;

use chrono::{Duration as ChronoDuration, Utc};
use gradatum_core::job::{CurateSpec, TERMINAL_PAYLOAD_KEEP};
use gradatum_core::paths::queue_db_path;
use gradatum_core::{
    Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TriggerSource,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use tempfile::TempDir;
use ulid::Ulid;

/// Non-terminal jobs: outside the command's remit.
const PENDING: u64 = 4;
/// Old terminal jobs — the fixture must overflow the keep window for the command to have work.
const TERMINAL_OLD: u64 = 215;
/// Recent terminal jobs, inside the retention cutoff of the first test.
const TERMINAL_RECENT: u64 = 5;

fn job_id(seq: u64) -> Ulid {
    Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq) + 1)
}

fn witness(seq: u64) -> String {
    format!("WITNESS-QUEUE-PURGE-{seq:04}")
}

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

/// Writes the fixture into `<root>/db/queue.sqlite` and returns the connection to drop
/// before the binary takes the file.
async fn populate(
    root: &Path,
    terminal: &[(u64, ChronoDuration)],
    pending: &[(u64, ChronoDuration)],
) {
    let db_path = queue_db_path(root);
    std::fs::create_dir_all(db_path.parent().expect("db dir")).expect("create db dir");
    let pool = QueueDb::open(&db_path).await.expect("open queue.sqlite");
    run_migrations(&pool).await.expect("migrations");
    let store = SqliteQueueStore::new(pool);
    for (seq, age) in pending {
        store
            .enqueue(record(*seq, JobStatus::Pending, *age))
            .await
            .expect("enqueue a non-terminal job");
    }
    for (seq, age) in terminal {
        store
            .enqueue(record(*seq, JobStatus::Done, *age))
            .await
            .expect("enqueue a terminal job");
    }
}

/// Runs `gradatum-admin queue purge …` and returns `(success, stdout, stderr)`.
fn run_purge(root: &Path, extra: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .arg("queue")
        .arg("purge")
        .arg("--root")
        .arg(root)
        .args(extra)
        .output()
        .expect("run gradatum-admin queue purge");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The `payload` column of every row, keyed by ULID.
async fn read_payloads(root: &Path) -> Vec<(String, String)> {
    let pool = QueueDb::open_existing(&queue_db_path(root))
        .await
        .expect("re-open queue.sqlite");
    pool.with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT id, payload FROM gradatum_jobs ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect()
    })
    .await
    .expect("read the payloads")
}

/// `(purged, retained)` witnesses, classified by what the payload column carries.
fn classify(payloads: &[(String, String)]) -> (Vec<u64>, Vec<u64>) {
    let seq_of = |id: &str| {
        (0..PENDING)
            .chain(100..(100 + TERMINAL_OLD))
            .chain(1000..(1000 + TERMINAL_RECENT))
            .find(|seq| job_id(*seq).to_string() == id)
            .unwrap_or_else(|| panic!("unknown row id {id}"))
    };
    let mut purged = Vec::new();
    let mut retained = Vec::new();
    for (id, payload) in payloads {
        let seq = seq_of(id);
        if payload.contains(&witness(seq)) {
            retained.push(seq);
        } else if payload.contains("PayloadPurged") {
            purged.push(seq);
        } else {
            panic!("row {seq}: payload neither carries its witness nor the sentinel");
        }
    }
    (purged, retained)
}

#[tokio::test]
async fn dry_run_designates_exactly_what_execution_erases() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    let expected = (TERMINAL_OLD + TERMINAL_RECENT) - TERMINAL_PAYLOAD_KEEP;
    assert!(
        expected > 0 && expected < TERMINAL_OLD,
        "fixture degenerate: {expected} payloads to purge — a missing purge would look green"
    );

    let terminal: Vec<(u64, ChronoDuration)> = (100..(100 + TERMINAL_OLD))
        .map(|seq| (seq, ChronoDuration::hours(48)))
        .chain((1000..(1000 + TERMINAL_RECENT)).map(|seq| (seq, ChronoDuration::zero())))
        .collect();
    let pending: Vec<(u64, ChronoDuration)> = (0..PENDING)
        .map(|seq| (seq, ChronoDuration::hours(48)))
        .collect();
    populate(root, &terminal, &pending).await;
    // The fixture is closed: the binary opens the file on its own from here on.

    // ── Dry-run: it designates, it does not act ──────────────────────────────────────
    let (ok, stdout, stderr) = run_purge(root, &["--dry-run"]);
    assert!(
        ok,
        "dry-run must succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(&format!("{expected} payload(s)")),
        "the dry-run must announce the {expected} targeted payloads, said:\n{stdout}"
    );

    let (purged, retained) = classify(&read_payloads(root).await);
    assert!(
        purged.is_empty(),
        "`--dry-run` erased {} payload(s) (rows {:?}) — a dry-run that acts is not a dry-run",
        purged.len(),
        purged
    );
    assert_eq!(
        retained.len() as u64,
        PENDING + TERMINAL_OLD + TERMINAL_RECENT,
        "no row lost its witness to the dry-run"
    );

    // ── Execution: the same rows, and only them ──────────────────────────────────────
    let (ok, stdout, stderr) = run_purge(root, &[]);
    assert!(
        ok,
        "execution must succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(&format!("{expected} payload(s)")),
        "the execution must report the {expected} purged payloads, said:\n{stdout}"
    );

    let (purged, retained) = classify(&read_payloads(root).await);
    assert_eq!(
        purged,
        (100..(100 + expected)).collect::<Vec<u64>>(),
        "the purged set must be exactly the old terminal rows outside the keep window"
    );
    for seq in 0..PENDING {
        assert!(
            retained.contains(&seq),
            "the non-terminal job {seq} lost its body — the command has no business there"
        );
    }
    for seq in (100 + expected)..(100 + TERMINAL_OLD) {
        assert!(
            retained.contains(&seq),
            "the terminal job {seq} is inside the keep window and must be untouched"
        );
    }
    for seq in 1000..(1000 + TERMINAL_RECENT) {
        assert!(
            retained.contains(&seq),
            "the recent terminal job {seq} is younger than the retention cutoff"
        );
    }

    // ── Idempotence: a second execution has nothing left to designate ────────────────
    let (ok, stdout, stderr) = run_purge(root, &[]);
    assert!(
        ok,
        "the second execution must succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("0 payload(s)"),
        "a second execution on the same queue purges nothing, said:\n{stdout}"
    );
}

#[tokio::test]
async fn older_than_narrows_the_cutoff_it_prints() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    // Everything is 90 minutes old: young enough to survive a `7d` cutoff, old enough to be
    // designated by a `1h` one. The keep window is overflowed so the eligibility turns on the
    // cutoff and not on the window.
    let terminal: Vec<(u64, ChronoDuration)> = (100..(100 + TERMINAL_OLD))
        .map(|seq| (seq, ChronoDuration::minutes(90)))
        .collect();
    populate(root, &terminal, &[]).await;

    let expected = TERMINAL_OLD - TERMINAL_PAYLOAD_KEEP;

    // Control first: too recent for the cutoff, so nothing may be designated — otherwise the
    // `1h` result below would only show that the command purges whatever it can.
    let (ok, stdout, stderr) = run_purge(root, &["--dry-run", "--older-than", "7d"]);
    assert!(
        ok,
        "dry-run must succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("0 payload(s)"),
        "`--older-than 7d` against 90-minute-old jobs must designate nothing, said:\n{stdout}"
    );

    let (ok, stdout, stderr) = run_purge(root, &["--dry-run", "--older-than", "1h"]);
    assert!(
        ok,
        "dry-run must succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(&format!("{expected} payload(s)")),
        "`--older-than 1h` must designate the {expected} payloads outside the keep window, \
         said:\n{stdout}"
    );

    let (ok, stdout, stderr) = run_purge(root, &["--older-than", "1h"]);
    assert!(
        ok,
        "execution must succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    let (purged, _) = classify(&read_payloads(root).await);
    assert_eq!(
        purged,
        (100..(100 + expected)).collect::<Vec<u64>>(),
        "the cutoff the command printed is the one it applied"
    );

    // A malformed window is refused, and refused before anything is written.
    let (ok, _, stderr) = run_purge(root, &["--older-than", "30"]);
    assert!(
        !ok,
        "`--older-than 30` (no unit) must be refused, not silently accepted"
    );
    assert!(
        stderr.contains("older-than"),
        "the refusal must name the offending option, said:\n{stderr}"
    );
}
