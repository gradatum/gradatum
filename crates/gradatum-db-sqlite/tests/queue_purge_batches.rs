//! Bounded-batch purge: the write lock is released between batches, and the batch count
//! follows from the number of rows to purge.
//!
//! A purge that selects every candidate, then rewrites the rows one by one inside a single
//! transaction, holds the queue's write lock for the whole loop: a concurrent writer waits
//! for the entire purge. This test pins the two properties that bound that wait:
//!
//! 1. **The batch count.** A purge of `N` rows runs `ceil(N / 500)` selection statements,
//!    one per batch — each batch being its own transaction, a selection statement is a
//!    batch. The count is read off the store's own connection with a SQLite trace hook,
//!    the observation point the workspace already uses for statement counts. It is taken
//!    with the queue to itself, so nothing depends on how a writer interleaved.
//! 2. **The measured wait.** A concurrent writer on its own connection times every write
//!    it performs; the longest wait it suffers while the purge runs must stay small **next
//!    to the purge itself** — under half of it — so that a lock held across the whole loop,
//!    which is what the wait would then approach, is excluded by the measurement rather
//!    than by a threshold picked in advance.
//!
//!    Two things make that criterion independent of how fast the host is. The purge spans
//!    several batches ([`PURGE_BATCHES`]), so it outlasts a single batch by that factor
//!    whatever the machine. And the **handshake**: the purge does not start until the probe
//!    has completed a write, so the window opens with a writer already contending — a host
//!    fast enough to finish the purge before the probe's thread was first scheduled cannot
//!    report an empty window as a small wait.
//!
//!    The probe writes on a sub-millisecond cadence and does **not** spin between two
//!    writes. Spinning was measured to write *less*: with no pause the probe completed 2
//!    writes over a purge where the cadence completed 40, on the same host and workload.
//!    A writer that re-takes the wake-up the instant the lock is released spends its time
//!    blocked inside SQLite and lands in fewer of the gaps between batches, and it also
//!    perturbs the purge's own truncating checkpoint, which needs a window of its own
//!    (observed once as `database is locked`). The pause is what makes the probe a
//!    concurrent writer rather than a competitor of the code under test.
//!
//!    What the probe can see is **calibrated** first: a deliberately held write
//!    transaction must show up in the measurement, otherwise a small number would only
//!    mean the probe was blind. That calibration is the anti-blindness proof, and it is
//!    deliberately *not* a count of the probe's writes: how many writes land inside a
//!    window is a property of the host — on a machine several times faster each batch
//!    holds the lock for less time and the probe lands in fewer gaps — so a floor set on a
//!    slow host reddens a fast one for its speed, on a test about the purge.
//!    `MIN_PROBE_WRITES` is therefore a floor of one, made structural by the handshake, and
//!    the count is printed rather than used as a criterion.
//!
//!    A second, deliberately loose absolute ceiling rides along: two seconds, far above
//!    any per-batch hold and far below a whole-loop hold on this workload. It catches the
//!    catastrophic case (a lock taken and never released) without making the test a
//!    measurement of the machine it runs on — on a capped CPU a batch is a large fraction
//!    of a second on its own, and a tight absolute bound would fail on the calendar
//!    rather than on the property.
//!
//! Two more tests measure what the physical guarantee costs, and both are **printed, not
//! asserted** — a threshold on the price of scrubbing would freeze a property that belongs
//! to SQLite, not to this queue. The first sizes the bytes a 1000-row rewrite sends to the
//! write-ahead log and the change in the database file, under each `secure_delete` setting
//! in turn. The second times a real `purge_terminal_payloads` of 1000 rows under the two
//! settings that are candidates for production, `ON` and `FAST`, so the report says what
//! the stronger guarantee costs in wall-clock time on this machine and not only in bytes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use gradatum_core::job::CurateSpec;
use gradatum_core::{
    Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TERMINAL_PAYLOAD_KEEP, TriggerSource,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use rusqlite::params;
use tempfile::TempDir;
use ulid::Ulid;

/// Rows one purge batch may carry — the bound this test holds the implementation to.
const BATCH_ROWS: u64 = 500;

/// Full batches the purge must span before the leftover row.
///
/// Eight, and not three: the criterion compares a per-batch hold to the whole purge, so the
/// purge must outlast one batch by this factor whatever the machine — and it must be long
/// enough in absolute terms that the probe meets it several times over. Both follow from
/// the batch count, which is what this constant sets. The price is wall-clock time on a
/// capped host (the workload rewrites `PURGEABLE` kilobytes of payload), and it is paid
/// here rather than by lowering what the test asks of the purge.
const PURGE_BATCHES: u64 = 8;

/// Rows the purge must reach: `PURGE_BATCHES` full batches, plus one row.
///
/// The leftover row is what makes the last batch a short one, so that the purge's
/// termination does not rest on an exact multiple of the batch size.
const PURGEABLE: u64 = PURGE_BATCHES * BATCH_ROWS + 1;

/// Marker carried by the batch selection statement, so that the trace hook can count
/// batches without counting anything else the connection runs.
const BATCH_MARKER: &str = "/* purge-batch */";

/// Absolute ceiling on the wait of a concurrent write — a **guard**, not the criterion.
///
/// The criterion is relative (`waited * 2 < purge_ms`, below): it holds whatever the
/// machine's speed, and it is what excludes a lock held across the loop. This ceiling only
/// catches the catastrophic case — a batch lock never released — and it is set two orders
/// of magnitude off the per-batch holds this workload produces (tens of milliseconds), so
/// that a slow, capped CPU cannot turn it red on its own.
const MAX_WAIT_MS: u64 = 2_000;

/// Wait the calibration transaction holds the write lock for.
const CALIBRATION_HOLD_MS: u64 = 300;

/// Writes the probe must have completed for the window to have had a writer in it.
///
/// One, and the handshake below is what makes it structural rather than lucky: the purge
/// starts only after the probe's first successful write of the window, and the counter is
/// not reset afterwards, so a window with no writer in it cannot reach this assertion.
///
/// It used to be two, on the reasoning that a single write was too thin to be a
/// measurement. It never was the measurement: what a green run needs is a probe that *can*
/// see a held lock, and the calibration establishes exactly that against a write lock held
/// for a known `CALIBRATION_HOLD_MS`. How many writes land inside a window is a count of
/// the host — on a runner several times faster than a capped dev host, each batch holds the
/// lock for a few tens of milliseconds, the probe lands in a couple of the gaps between
/// them, and a floor of two reddens the fast host for its speed on a test about the purge.
///
/// The count is still printed, and it still has to be at least one: an implementation
/// holding the lock across the whole loop makes *every* write wait for the loop, so the
/// wait it produces is the purge's own duration and the relative criterion below — the one
/// that decides — turns red.
const MIN_PROBE_WRITES: usize = 1;

/// Bytes of body each row of the batched-purge test carries.
///
/// The purge's per-row work is dominated by the body it carries — read, deserialise,
/// rewrite, write back — so a run of rows this size takes long enough for the concurrent
/// writer below to sample the lock repeatedly. On rows of a few dozen bytes the purge would
/// be over in a couple of milliseconds, and a bound asserted on a window that short would
/// hold even if the lock were never released: a single batch's hold is what the bound has
/// to exclude, so the whole loop has to outlast it several times over.
const BODY_BYTES: usize = 1024;

/// Pause between two writes of the concurrent probe — under a millisecond, and not zero.
///
/// Not zero because a probe that spins writes *less* than one that pauses: measured against
/// this workload, 2 writes inside the window with no pause against 40 with this cadence, on
/// the same host. Spinning keeps the probe inside SQLite's busy handler — where it is
/// blocked, has nothing to report, and competes for the wake-up the purge's own truncating
/// checkpoint needs. Under a millisecond because the pause must stay short next to the
/// holds it is there to measure: a batch is tens of milliseconds, so the probe still asks
/// for the lock many times over one batch and cannot sleep through a purge.
const PROBE_CADENCE: Duration = Duration::from_micros(200);

/// How long the probe is given to complete its first write before the purge may start.
///
/// A handshake that never resolves is a broken probe, not a slow host — the probe's own
/// write is a single `INSERT` on an idle queue — so the patience is generous and its
/// expiry is a failure, reported as such.
const PROBE_START_PATIENCE: Duration = Duration::from_secs(10);

/// How long the probe waits for the write lock before giving up on a write.
///
/// Deliberately longer than the ceiling under test: a write that errors out is a different
/// event from a write that waited, and it would abort the probe instead of measuring the
/// wait it was meant to measure. The probe has to outlast the worst case it is there to
/// catch — the whole-loop hold — for that hold to be reported as a number.
const PROBE_BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Serialises the two tests that read the counters below.
///
/// `Connection::trace_v2` takes a function pointer, so the counters are process statics —
/// and `cargo test` runs the tests of one binary on several threads by default. Two tests
/// of this binary install the hook, on two different databases: without this lock the
/// counter one of them resets is the counter the other is incrementing, and the batch count
/// of a purge is then the sum of two purges, read at different moments. It held only
/// accidentally before — the two purges ran the same number of batches, so an interleaving
/// happened to be invisible.
static TRACED_QUEUE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How many batch selections the store ran. `Connection::trace_v2` takes a function
/// pointer, so the state is a process static; the tests that read it hold [`TRACED_QUEUE`],
/// and only the connection that installed the hook increments the counter.
static BATCH_SELECTS: AtomicUsize = AtomicUsize::new(0);

/// How many transactions the store opened. Read from the `BEGIN` statements the traced
/// connection ran, which is the count the criterion asks for whatever the implementation
/// calls a batch: the transactions are what a concurrent writer waits on.
static PURGE_TXNS: AtomicUsize = AtomicUsize::new(0);

/// Counts, off the traced statements, the two things the criterion counts: the selection
/// statement of each batch, and the transaction each batch opens.
fn count_purge_statements(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_stmt, sql) = event {
        let sql = sql.trim_start();
        if sql.contains(BATCH_MARKER) {
            BATCH_SELECTS.fetch_add(1, Ordering::SeqCst);
        }
        if sql.starts_with("BEGIN") {
            PURGE_TXNS.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Explicitly ordered ULID of job `seq` — deterministic, so "the last 200 terminal rows"
/// does not depend on the ULID generator's clock.
fn job_id(seq: u64) -> Ulid {
    Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq) + 1)
}

/// Terminal `Done` job carrying a body of `body_bytes` bytes — the unit of work the purge
/// deserialises, rewrites and writes back.
fn terminal_record(seq: u64, body_bytes: usize) -> JobRecord {
    let now = Utc::now();
    JobRecord {
        id: job_id(seq),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                body: Some(format!("body-{seq:05}-{}", "x".repeat(body_bytes))),
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
            status: JobStatus::Done,
            created_at: now,
            started_at: None,
            completed_at: Some(now),
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

/// A concurrent writer on its own connection, timing every write it performs.
///
/// It writes `Pending` rows: a concurrent writer in production is enqueueing work, and a
/// `Pending` row is never a purge candidate, so the probe cannot change what the purge
/// has to do.
///
/// It writes on [`PROBE_CADENCE`] — short next to the holds it measures, and not zero: see
/// that constant for why a spinning probe is the weaker of the two.
struct WriteProbe {
    stop: Arc<AtomicBool>,
    first_write: Arc<AtomicBool>,
    max_wait_ms: Arc<AtomicU64>,
    writes: Arc<AtomicUsize>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WriteProbe {
    fn start(path: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let first_write = Arc::new(AtomicBool::new(false));
        let max_wait_ms = Arc::new(AtomicU64::new(0));
        let writes = Arc::new(AtomicUsize::new(0));

        let (stop_thread, first_thread, max_thread, writes_thread) = (
            Arc::clone(&stop),
            Arc::clone(&first_write),
            Arc::clone(&max_wait_ms),
            Arc::clone(&writes),
        );
        let path = path.to_path_buf();

        let handle = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&path).expect("probe connection");
            conn.busy_timeout(PROBE_BUSY_TIMEOUT)
                .expect("probe busy_timeout");
            while !stop_thread.load(Ordering::SeqCst) {
                let started = Instant::now();
                let inserted = conn.execute(
                    "INSERT INTO gradatum_jobs \
                     (id, payload, status, priority, class, created_at, scheduled_at, \
                      attempt_count, tenant_id) \
                     VALUES (?1, '{}', 'Pending', 2, 'System', ?2, ?2, 0, 'main')",
                    params![Ulid::generate().to_string(), Utc::now().to_rfc3339()],
                );
                inserted.expect("a concurrent enqueue must not fail");
                let waited = started.elapsed().as_millis() as u64;
                max_thread.fetch_max(waited, Ordering::SeqCst);
                writes_thread.fetch_add(1, Ordering::SeqCst);
                // Published last, so that a caller waiting on the handshake below is
                // waiting on a write that finished, not on one that started.
                first_thread.store(true, Ordering::SeqCst);
                std::thread::sleep(PROBE_CADENCE);
            }
        });

        Self {
            stop,
            first_write,
            max_wait_ms,
            writes,
            handle: Some(handle),
        }
    }

    fn max_wait_ms(&self) -> u64 {
        self.max_wait_ms.load(Ordering::SeqCst)
    }

    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    /// Arms a fresh window: the wait ceiling and the write count go back to zero, and the
    /// first-write flag is cleared so that [`Self::wait_for_first_write`] afterwards waits
    /// for a write of the **new** window, not for one the calibration already produced.
    fn reset(&self) {
        self.first_write.store(false, Ordering::SeqCst);
        self.max_wait_ms.store(0, Ordering::SeqCst);
        self.writes.store(0, Ordering::SeqCst);
    }

    /// Waits, within `patience`, for the probe to complete a write of the current window,
    /// returning whether one landed.
    ///
    /// This is the handshake that opens the purge window, and it is what the floor of one
    /// write rests on: the window then holds a writer by construction, whatever the host's
    /// speed, so the wait the probe reports cannot be a wait it never had the occasion to
    /// suffer.
    fn wait_for_first_write(&self, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        while !self.first_write.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }
}

impl Drop for WriteProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("probe thread joins");
        }
    }
}

/// A queue holding `PURGEABLE` terminal rows plus the keep window, on a temp file, with the
/// statement trace installed on the store's own connection.
///
/// The directory is returned with the rest: dropping it removes the database under the
/// connections that are still open on it.
async fn queue_of_terminal_rows() -> (TempDir, PathBuf, QueueDb, SqliteQueueStore) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("queue.sqlite");

    let db = QueueDb::open(&path).await.expect("open queue.sqlite");
    run_migrations(&db).await.expect("migrations");
    let store = SqliteQueueStore::new(db.clone());

    let total = PURGEABLE + TERMINAL_PAYLOAD_KEEP;
    for seq in 0..total {
        store
            .enqueue(terminal_record(seq, BODY_BYTES))
            .await
            .expect("enqueue through the real path");
    }

    // The observation point: the store's own connection runs the purge statements.
    db.with_conn(|conn| {
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(count_purge_statements),
        );
        Ok(())
    })
    .await
    .expect("installing the trace hook");

    (dir, path, db, store)
}

/// `N` rows reach the purge, and it selects them batch by batch: `ceil(N / 500)` selection
/// statements, each in its own transaction.
///
/// This half runs with the queue to itself, so what it counts cannot depend on how a
/// concurrent writer happened to interleave: a count of nine is nine, or it is not.
#[tokio::test]
async fn purge_runs_one_selection_per_batch() {
    let _exclusive = TRACED_QUEUE.lock().await;
    let (_dir, _path, _db, store) = queue_of_terminal_rows().await;
    let total = PURGEABLE + TERMINAL_PAYLOAD_KEEP;

    BATCH_SELECTS.store(0, Ordering::SeqCst);
    PURGE_TXNS.store(0, Ordering::SeqCst);
    let purged = store
        .purge_terminal_payloads(None)
        .await
        .expect("purge_terminal_payloads");
    let batch_selects = BATCH_SELECTS.load(Ordering::SeqCst);
    let txns = PURGE_TXNS.load(Ordering::SeqCst);
    let expected = PURGEABLE.div_ceil(BATCH_ROWS) as usize;

    assert_eq!(
        purged, PURGEABLE,
        "{total} terminal rows − {TERMINAL_PAYLOAD_KEEP} kept = {PURGEABLE} purged"
    );
    assert_eq!(
        txns, expected,
        "one transaction per batch: {PURGEABLE} rows must be written in \
         ceil({PURGEABLE} / {BATCH_ROWS}) = {expected} transactions, the store opened {txns}"
    );
    assert_eq!(
        batch_selects, expected,
        "one selection statement per batch: {PURGEABLE} rows must run {expected} batches, \
         ran {batch_selects}"
    );
    eprintln!(
        "purge batches: {purged} rows purged in {txns} transactions and {batch_selects} \
         selection statements of at most {BATCH_ROWS} rows"
    );
}

/// The wait a concurrent writer suffers stays under the bound, over a purge long enough
/// that a lock held across the whole loop would exceed it many times over.
#[tokio::test]
async fn purge_bounds_the_wait_of_a_concurrent_write() {
    let _exclusive = TRACED_QUEUE.lock().await;
    let (_dir, path, _db, store) = queue_of_terminal_rows().await;

    let probe = WriteProbe::start(&path);
    probe.reset();

    // Calibration: the probe must be able to see a held write lock. Without this half, a
    // small measurement below could mean "nothing blocked" or "the probe measures
    // nothing", and the two are not the same claim.
    {
        let blocker = rusqlite::Connection::open(&path).expect("blocking connection");
        blocker
            .busy_timeout(Duration::from_millis(5000))
            .expect("blocker busy_timeout");
        blocker
            .execute_batch(
                "BEGIN IMMEDIATE; \
                 INSERT INTO gradatum_jobs \
                 (id, payload, status, priority, class, created_at, scheduled_at, \
                  attempt_count, tenant_id) \
                 VALUES ('01ARZ3NDEKTSV4RRFFQ69G5FAV', '{}', 'Pending', 2, 'System', \
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 0, 'main');",
            )
            .expect("holding the write lock");
        std::thread::sleep(Duration::from_millis(CALIBRATION_HOLD_MS));
        blocker
            .execute_batch("COMMIT;")
            .expect("releasing the write lock");
    }

    // The probe records a wait when the write it was blocked on returns, not when the lock
    // is released: reading the moment the lock goes away samples a value the probe may not
    // have stored yet, and a zero there means "not read yet", not "not blocked". The
    // reading is waited for, within a bound — a bound that expires is the failure the
    // assertion below reports.
    let calibration_deadline = Instant::now() + Duration::from_secs(5);
    while probe.max_wait_ms() < CALIBRATION_HOLD_MS - 50 && Instant::now() < calibration_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let calibrated = probe.max_wait_ms();
    assert!(
        calibrated >= CALIBRATION_HOLD_MS - 50,
        "the probe measured {calibrated} ms against a write lock held for \
         {CALIBRATION_HOLD_MS} ms — it cannot see blocking, so the measurement below \
         would be green by blindness"
    );

    // The window the criterion is read over opens here, and it opens with a writer in it:
    // the purge waits for the probe's first write of the window. On a host fast enough to
    // finish the whole purge before the probe's thread was first scheduled, the measurement
    // would otherwise be of an empty window, and a wait of zero over a purge of zero is a
    // green that says nothing.
    probe.reset();
    assert!(
        probe.wait_for_first_write(PROBE_START_PATIENCE),
        "the probe completed no write in {PROBE_START_PATIENCE:?} — the purge must not \
         start before a concurrent write has landed, or the wait measured below is the wait \
         of a window the probe was never in"
    );

    BATCH_SELECTS.store(0, Ordering::SeqCst);

    let purge_started = Instant::now();
    let purged = store
        .purge_terminal_payloads(None)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "purge_terminal_payloads failed after {} of {} batches: {e}",
                BATCH_SELECTS.load(Ordering::SeqCst),
                PURGEABLE.div_ceil(BATCH_ROWS)
            )
        });
    let purge_ms = purge_started.elapsed().as_millis() as u64;

    let batch_selects = BATCH_SELECTS.load(Ordering::SeqCst);
    let waited = probe.max_wait_ms();
    let writes = probe.writes();

    // Printed before anything is asserted, so a failure still shows the figures that
    // produced it rather than only the sentence that rejected them.
    eprintln!(
        "purge wait: {purged} rows purged in {batch_selects} batches over {purge_ms} ms; \
         longest concurrent-write wait {waited} ms over {writes} writes \
         (waited*2 < purge {purge_ms} ms, absolute ceiling {MAX_WAIT_MS} ms)"
    );

    assert_eq!(
        purged, PURGEABLE,
        "the purge reached the rows it must reach"
    );
    assert!(
        writes >= MIN_PROBE_WRITES,
        "the probe completed {writes} writes over a {purge_ms} ms purge ({batch_selects} \
         batches) — the handshake guarantees at least one, so the window lost its writer"
    );
    // The absolute guard: catastrophic only. A lock held across the whole loop and never
    // released would leave a write waiting here until the probe's own busy timeout.
    assert!(
        waited < MAX_WAIT_MS,
        "a concurrent write waited {waited} ms for the purge — the write lock must be \
         released between batches, so no wait may approach the {MAX_WAIT_MS} ms ceiling \
         ({writes} concurrent writes over a {purge_ms} ms purge in {batch_selects} batches)"
    );
    // The criterion is this one, and it is relative: a lock held across the whole loop
    // would have made one of the probe's writes wait for the loop itself, which is a large
    // fraction of the purge, while a per-batch hold is a fraction of that. Comparing the
    // wait to the purge keeps the test honest on a slow machine — where both grow — and
    // sharp on a fast one, which a threshold in milliseconds could not do.
    assert!(
        waited.saturating_mul(2) < purge_ms,
        "the longest wait ({waited} ms) is not small next to the whole purge ({purge_ms} ms) \
         — a wait of that size cannot be told apart from a lock held across the loop"
    );
}

/// Size in bytes of a file, or 0 when it does not exist.
fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Rows the amplification measurement rewrites.
const AMPLIFICATION_ROWS: u64 = 1000;

/// Bytes of body each row of the amplification measurement carries — a few hundred, the
/// size a queue row actually holds.
const AMPLIFICATION_BODY_BYTES: usize = 200;

/// Runs a truncating checkpoint by hand, so that a log size read afterwards measures the
/// workload that followed and nothing that came before it.
///
/// The row `wal_checkpoint` reports is deliberately not read. On this SQLite it answers
/// `0|0|0` — on a connection that has just checkpointed and truncated a full log, and on
/// this machine's `sqlite3` CLI as well — so the frame count it offers cannot be told apart
/// from an empty log, and a figure built on it would be a zero that proves nothing. The
/// file answers instead: the checkpoint truncates it to nothing, and the size it reaches
/// before a checkpoint is what the workload wrote into it.
async fn truncate_log(db: &QueueDb) {
    db.with_conn(|conn| conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);"))
        .await
        .expect("a hand-run checkpoint");
}

/// Price of scrubbing: what the same rewrite costs, under each `secure_delete` setting.
///
/// The three settings are the three behaviours — `OFF` leaves the bytes of a rewritten cell
/// in the page that held it, `FAST` scrubs them there but not the whole pages it frees,
/// `ON` scrubs those too — and the price is the same workload measured on each: 1200
/// terminal rows through the store's enqueue path, then a rewrite of the 1000 oldest
/// payloads with the statement the purge uses and the payload shape the purge writes. Only
/// the setting differs.
///
/// The rewrite is run here rather than through `purge_terminal_payloads` because the purge
/// ends with its own truncating checkpoint: the log it wrote is gone by the time it
/// returns, and the log it wrote is what this measures. What the purge itself does is
/// pinned by the two tests above.
///
/// The log is emptied after the inserts, so the size read after the rewrite is the rewrite's
/// alone and the inserts — identical on both sides — cannot hide the difference. Automatic
/// checkpointing is raised out of the way for the same reason: SQLite checkpoints by itself
/// once the log passes 1000 pages, and a checkpoint resets the log, so the file would stop
/// following the workload.
#[tokio::test]
async fn purge_write_amplification_is_measured_with_and_without_secure_delete() {
    for (label, secure_delete) in [("OFF", 0i64), ("FAST", 2i64), ("ON", 1i64)] {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("queue.sqlite");
        let wal = PathBuf::from(format!("{}-wal", path.display()));

        let db = QueueDb::open(&path).await.expect("open queue.sqlite");
        run_migrations(&db).await.expect("migrations");
        // The connection opens with `secure_delete = ON`; the loop moves it to each setting
        // under measurement, `OFF` included, so the three are compared on one workload.
        let journal_mode: String = db
            .with_conn(move |conn| {
                conn.pragma_update(None, "secure_delete", secure_delete)?;
                // Raised, not zeroed: on this SQLite the threshold is what keeps the log,
                // and a threshold of zero means "on every commit" rather than "never".
                conn.pragma_update(None, "wal_autocheckpoint", 100_000i64)?;
                conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            })
            .await
            .expect("selecting the setting under measurement");
        assert_eq!(
            journal_mode, "wal",
            "{label}: the size below is read from the write-ahead log"
        );
        let store = SqliteQueueStore::new(db.clone());

        let total = AMPLIFICATION_ROWS + TERMINAL_PAYLOAD_KEEP;
        for seq in 0..total {
            store
                .enqueue(terminal_record(seq, AMPLIFICATION_BODY_BYTES))
                .await
                .expect("enqueue through the real path");
        }

        let inserted_log_bytes = size(&wal);
        truncate_log(&db).await;
        assert_eq!(
            size(&wal),
            0,
            "{label}: the log is emptied before the rewrite, so the size read after it is \
             the rewrite's alone"
        );

        // The rows the purge reaches: every row is terminal and the keep window is the 200
        // most recent, so the 1000 oldest rows are exactly its candidates. The rewritten
        // payload is built the way the purge builds it — the record is deserialised, the
        // kind replaced, the record serialised again.
        let rewritten: Vec<(String, String)> = db
            .with_conn(|conn| {
                let mut stmt =
                    conn.prepare("SELECT id, payload FROM gradatum_jobs ORDER BY id LIMIT ?1")?;
                let rows = stmt
                    .query_map(params![AMPLIFICATION_ROWS as i64], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let mut rewritten = Vec::with_capacity(rows.len());
                for (id, payload) in rows {
                    let mut record: JobRecord = serde_json::from_str(&payload)
                        .expect("a payload written by the store deserialises");
                    record.spec.kind = Job::PayloadPurged;
                    rewritten.push((id, serde_json::to_string(&record).expect("reserialises")));
                }
                Ok(rewritten)
            })
            .await
            .expect("reading the rows to rewrite");
        assert_eq!(rewritten.len() as u64, AMPLIFICATION_ROWS);

        let main_before = size(&path);

        // One transaction, the shape the purge had before it was batched: the measurement
        // isolates the setting, not the transactional shape.
        db.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            {
                let mut update =
                    tx.prepare_cached("UPDATE gradatum_jobs SET payload = ?1 WHERE id = ?2")?;
                for (id, payload) in rewritten {
                    update.execute(params![payload, id])?;
                }
            }
            tx.commit()
        })
        .await
        .expect("the measured rewrite");

        let rewrite_log_bytes = size(&wal);
        let main_after = size(&path);
        // Control: the rewrite wrote to the log. A zero here is a blind measurement — a log
        // not followed, a checkpoint in the way — not a rewrite that costs nothing, and the
        // two settings would then be compared as two zeroes.
        assert!(
            rewrite_log_bytes > 0,
            "{label}: {AMPLIFICATION_ROWS} rows rewritten sent nothing to the log, while \
             the {total} inserts before them left {inserted_log_bytes} B — the measurement \
             is not reading the workload"
        );
        let page_size: i64 = db
            .with_conn(|conn| conn.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0)))
            .await
            .expect("page size");

        eprintln!(
            "purge amplification [{label}]: {AMPLIFICATION_ROWS} rows rewritten wrote \
             {rewrite_log_bytes} B to the log ({} pages at {page_size} B/page) — main \
             {main_before} → {main_after} B ({:+} B); the {total} inserts before them left \
             {inserted_log_bytes} B",
            rewrite_log_bytes / page_size.max(1) as u64,
            main_after as i64 - main_before as i64,
        );
    }
}

/// Rows the purge-time measurement purges — the same thousand the measurement above
/// rewrites, so the two figures describe the same work.
const PURGE_TIME_ROWS: u64 = 1000;

/// What the stronger guarantee costs **in time**: a real `purge_terminal_payloads` of 1000
/// rows, run under `ON` and under `FAST`.
///
/// The measurement above isolates the setting on one rewrite, and has to run that rewrite
/// by hand because the purge ends with its own truncating checkpoint — the log it wrote is
/// gone by the time it returns. This one measures the purge itself, at the price of not
/// seeing inside it: what is read is the wall-clock time of the call, the size of the
/// database file around it (the zeroing writes are what `ON` costs in volume), and the
/// write-ahead log, which the purge's checkpoint must leave empty in both cases.
///
/// Printed, not asserted, for the same reason: the cost in milliseconds belongs to the
/// machine. The one assertion is the control — the purge reached the 1000 rows outside the
/// keep window — without which a fast figure could just be a purge that did nothing.
#[tokio::test]
async fn purge_time_for_1000_rows_is_measured_under_on_and_fast() {
    for (label, secure_delete) in [("FAST", 2i64), ("ON", 1i64)] {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("queue.sqlite");
        let wal = PathBuf::from(format!("{}-wal", path.display()));

        let db = QueueDb::open(&path).await.expect("open queue.sqlite");
        run_migrations(&db).await.expect("migrations");
        db.with_conn(move |conn| conn.pragma_update(None, "secure_delete", secure_delete))
            .await
            .expect("selecting the setting under measurement");
        let store = SqliteQueueStore::new(db.clone());

        let total = PURGE_TIME_ROWS + TERMINAL_PAYLOAD_KEEP;
        for seq in 0..total {
            store
                .enqueue(terminal_record(seq, AMPLIFICATION_BODY_BYTES))
                .await
                .expect("enqueue through the real path");
        }

        let log_before = size(&wal);
        let main_before = size(&path);

        let started = Instant::now();
        let purged = store
            .purge_terminal_payloads(None)
            .await
            .expect("purge_terminal_payloads");
        let purge_ms = started.elapsed().as_millis() as u64;

        // Control: the purge did the work being timed. A figure read next to a purge that
        // reached nothing would describe an idle call, not the guarantee.
        assert_eq!(
            purged, PURGE_TIME_ROWS,
            "{label}: {total} terminal rows − {TERMINAL_PAYLOAD_KEEP} kept = \
             {PURGE_TIME_ROWS} purged, the timed purge reached {purged}"
        );

        let log_after = size(&wal);
        let main_after = size(&path);
        assert_eq!(
            log_after,
            0,
            "{label}: the purge ends with a truncating checkpoint — {} byte(s) left in {}",
            log_after,
            wal.display()
        );
        eprintln!(
            "purge time [{label}]: {purged} rows purged in {purge_ms} ms — log \
             {log_before} → {log_after} B, main {main_before} → {main_after} B ({:+} B)",
            main_after as i64 - main_before as i64,
        );
    }
}
