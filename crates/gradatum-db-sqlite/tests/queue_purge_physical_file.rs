//! Physical erasure of a purged payload, measured on the bytes of the queue file.
//!
//! The purge shipped earlier is **logical**: it rewrites `spec.kind` into a tombstone and
//! leaves the row in place. The column no longer carries the body — but the *bytes* may
//! survive in the file, in space SQLite freed without scrubbing. This test is the
//! physical counterpart: it asks whether a body can still be **found in the bytes** once
//! the purge and its checkpoint have run.
//!
//! # Protocol
//!
//! One terminal job per witness, each witness a unique 32-hex-character token carried by
//! the note body (`CurateSpec.body`), written into a `queue.sqlite` under a `TempDir`.
//! Purge. Then the raw bytes of `queue.sqlite` and of its `-wal` sidecar are read — while
//! the connection is still open, so that what is measured is the state the purge leaves
//! behind, not the state a connection close would leave behind.
//!
//! # The control that keeps the assertion honest
//!
//! The purge only targets rows **outside** the keep window, so witnesses are split in two
//! groups and both are checked: every kept witness must still appear in the bytes, and no
//! purged witness may. Without the first half, the second would be green on a file that
//! is empty, unreadable, or simply not the file being purged.
//!
//! A shared witness for all jobs would prove nothing — the kept rows carry it, so the
//! check would be green by construction. Each job therefore carries its own.
//!
//! # The body that does not fit in one page
//!
//! A small body is scrubbing's easy case: the rewrite frees a few bytes inside the page
//! that held it, and any setting that overwrites deleted content covers it. `CurateSpec.body`
//! is **not bounded**, so the case that matters is the other one: a body of several pages,
//! which SQLite stores in **overflow pages** it frees wholesale. A setting that scrubs only
//! the space freed inside the page where the deletion happens — `secure_delete = FAST` —
//! hands those overflow pages back with their bytes intact, and the body survives the purge
//! in the file. The second test below is that case, with the witness placed beyond the
//! first page of a three-page body so that it can only lie in an overflow page. It is the
//! test that tells `ON` from `FAST`; the first one does not.

use std::path::{Path, PathBuf};

use chrono::Utc;
use gradatum_core::job::CurateSpec;
use gradatum_core::{
    Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TriggerSource,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use tempfile::TempDir;
use ulid::Ulid;

/// Terminal rows written: the keep window plus the rows the purge is expected to reach.
const TOTAL: u64 = 250;
/// Rows outside the keep window — the ones whose body must disappear.
const PURGED: u64 = 50;

/// Unique 32-hex-character witness of job `seq`.
///
/// Hex, fixed width, and derived from the sequence so that the mapping job → witness is
/// stable across runs; 32 characters is wide enough that a hit in the raw bytes cannot be
/// a coincidence of unrelated text.
fn witness(seq: u64) -> String {
    let mixed = (seq.wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ 0x5DEE_CE66_D2B1_9F1B;
    format!("{mixed:032x}")
}

/// Explicitly ordered ULID of job `seq`, so that "the last 200 terminal rows" is
/// deterministic without depending on the ULID generator's clock.
fn job_id(seq: u64) -> Ulid {
    Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq) + 1)
}

/// Terminal `Done` job carrying `witness(seq)` in its note body.
fn terminal_record(seq: u64) -> JobRecord {
    terminal_record_with_body(seq, witness(seq))
}

/// Terminal `Done` job carrying `body` as the note body of its `CurateSpec`.
fn terminal_record_with_body(seq: u64, body: String) -> JobRecord {
    let now = Utc::now();
    JobRecord {
        id: job_id(seq),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                body: Some(body),
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

/// Raw bytes of the database file and of its `-wal` sidecar, decoded lossily.
///
/// Lossy decoding is the right tool here: the question is whether a plain ASCII witness is
/// *present* in the bytes, and a decode that dropped invalid sequences could hide it.
fn raw_bytes(db_path: &Path) -> Vec<(String, String)> {
    let mut files = vec![db_path.to_path_buf()];
    let wal = PathBuf::from(format!("{}-wal", db_path.display()));
    if wal.exists() {
        files.push(wal);
    }
    files
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).expect("reading a file of the queue database");
            let name = path
                .file_name()
                .expect("a file name")
                .to_string_lossy()
                .into_owned();
            (name, String::from_utf8_lossy(&bytes).into_owned())
        })
        .collect()
}

#[tokio::test]
async fn purged_bodies_are_absent_from_the_raw_bytes_after_the_purge() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("queue.sqlite");

    let db = QueueDb::open(&path).await.expect("open queue.sqlite");
    run_migrations(&db).await.expect("migrations");
    let store = SqliteQueueStore::new(db.clone());

    for seq in 0..TOTAL {
        store
            .enqueue(terminal_record(seq))
            .await
            .expect("enqueue through the real path, never a literal INSERT");
    }

    let purged = store
        .purge_terminal_payloads(None)
        .await
        .expect("purge_terminal_payloads");
    assert_eq!(
        purged, PURGED,
        "the purge reaches the rows outside the keep window"
    );

    // The checkpoint the purge owes the caller: without it the pre-rewrite page images
    // stay in the write-ahead log, so the body is still reachable in the bytes even after
    // the column was rewritten. Observed through the sidecar itself: a truncating
    // checkpoint leaves an empty `-wal` behind.
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        wal_len,
        0,
        "after the purge the write-ahead log must be checkpointed and truncated — \
         {} byte(s) left in {}",
        wal_len,
        wal.display()
    );

    let files = raw_bytes(&path);
    let whole: String = files.iter().map(|(_, text)| text.as_str()).collect();

    // Control: the kept bodies are still there. A green second half on a file that holds
    // nothing (empty file, wrong path, encode/decode mismatch) would prove nothing.
    for seq in PURGED..TOTAL {
        let w = witness(seq);
        assert!(
            whole.contains(&w),
            "the witness of kept row #{seq} must still be present in the bytes — \
             without it, the absence below would be vacuous"
        );
    }

    for (name, text) in &files {
        for seq in 0..PURGED {
            let w = witness(seq);
            assert!(
                !text.contains(&w),
                "the witness of purged row #{seq} is still readable in the raw bytes of \
                 {name} — the purge is logical, not physical"
            );
        }
    }
}

// ── The body larger than a page ───────────────────────────────────────────────

/// Bytes of body each row of the overflow test carries — three 4 096-byte pages, so the
/// body cannot be held in the row's own page and SQLite spreads it over overflow pages.
const OVERFLOW_BODY_BYTES: usize = 3 * 4096;

/// Offset, inside the body, at which the witness starts — two pages in, well past the
/// inline limit of a value (under one page), so the witness can only lie in an overflow
/// page. The test checks that against the database's real `page_size` rather than assuming
/// it, and that check is what makes the case discriminating.
const OVERFLOW_WITNESS_OFFSET: usize = 2 * 4096;

/// Filler of the overflow bodies.
///
/// Letters and hyphens only, no digit: a witness is 32 hexadecimal characters, so this
/// text can never spell one — a hit in the raw bytes is the witness itself, never the
/// padding around it.
const OVERFLOW_FILLER: &str = "overflow-filler-for-a-body-larger-than-one-page-";

/// Body of [`OVERFLOW_BODY_BYTES`] bytes carrying `witness(seq)` at
/// [`OVERFLOW_WITNESS_OFFSET`].
fn overflow_body(seq: u64) -> String {
    let mut body = String::with_capacity(OVERFLOW_BODY_BYTES);
    while body.len() < OVERFLOW_WITNESS_OFFSET {
        let take = (OVERFLOW_WITNESS_OFFSET - body.len()).min(OVERFLOW_FILLER.len());
        body.push_str(&OVERFLOW_FILLER[..take]);
    }
    body.push_str(&witness(seq));
    while body.len() < OVERFLOW_BODY_BYTES {
        let take = (OVERFLOW_BODY_BYTES - body.len()).min(OVERFLOW_FILLER.len());
        body.push_str(&OVERFLOW_FILLER[..take]);
    }
    body
}

/// Terminal rows written by the overflow test: the keep window plus the rows the purge
/// must reach.
const OVERFLOW_TOTAL: u64 = 250;

/// Rows outside the keep window — the ones whose body must disappear from the overflow
/// pages.
const OVERFLOW_PURGED: u64 = 50;

/// A body of three pages is erased from the overflow pages that held it, not only from the
/// page the rewrite touched.
///
/// Same protocol as the small-body case, and the same two halves: the kept witnesses must
/// still be readable in the overflow pages — they are the control that proves the region is
/// read — and no purged witness may be.
#[tokio::test]
async fn a_body_larger_than_a_page_is_erased_from_its_overflow_pages() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("queue.sqlite");

    let db = QueueDb::open(&path).await.expect("open queue.sqlite");
    run_migrations(&db).await.expect("migrations");
    let store = SqliteQueueStore::new(db.clone());

    // The claim this test rests on, checked against the database in hand rather than
    // assumed: the witness lies beyond the inline part of the value, in an overflow page.
    let page_size: i64 = db
        .with_conn(|conn| conn.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0)))
        .await
        .expect("page size");
    assert!(
        OVERFLOW_WITNESS_OFFSET as i64 >= page_size,
        "the witness starts at offset {OVERFLOW_WITNESS_OFFSET} of the body while a page \
         holds {page_size} bytes — below the inline limit, it would sit in the row's own \
         page and this test would prove no more than the small-body case above"
    );
    assert!(
        OVERFLOW_BODY_BYTES as i64 > 2 * page_size,
        "the body is {OVERFLOW_BODY_BYTES} bytes over a {page_size}-byte page: it must span \
         more than two pages for the value to be stored out of line"
    );

    for seq in 0..OVERFLOW_TOTAL {
        store
            .enqueue(terminal_record_with_body(seq, overflow_body(seq)))
            .await
            .expect("enqueue through the real path, never a literal INSERT");
    }

    let purged = store
        .purge_terminal_payloads(None)
        .await
        .expect("purge_terminal_payloads");
    assert_eq!(
        purged, OVERFLOW_PURGED,
        "the purge reaches the rows outside the keep window"
    );

    // Same checkpoint demand as the small-body case: without it the pre-rewrite images of
    // the overflow pages would still be in the log.
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        wal_len,
        0,
        "after the purge the write-ahead log must be checkpointed and truncated — \
         {} byte(s) left in {}",
        wal_len,
        wal.display()
    );

    let files = raw_bytes(&path);
    let whole: String = files.iter().map(|(_, text)| text.as_str()).collect();

    // Control: the kept bodies still appear — at the same offset, in the same overflow
    // pages. Without it, the absence below could mean "the overflow region is not read by
    // this measurement" instead of "the purge erased it".
    for seq in OVERFLOW_PURGED..OVERFLOW_TOTAL {
        let w = witness(seq);
        assert!(
            whole.contains(&w),
            "the witness of kept row #{seq} must still be present in the bytes — it sits in \
             an overflow page of a {OVERFLOW_BODY_BYTES}-byte body, so its presence is what \
             proves those pages are read at all"
        );
    }

    for (name, text) in &files {
        for seq in 0..OVERFLOW_PURGED {
            let w = witness(seq);
            assert!(
                !text.contains(&w),
                "the witness of purged row #{seq} is still readable in the raw bytes of \
                 {name} — it sits past offset {OVERFLOW_WITNESS_OFFSET} of a \
                 {OVERFLOW_BODY_BYTES}-byte body, in an overflow page: `secure_delete=FAST` \
                 would leave it there, only `ON` scrubs the pages a statement frees"
            );
        }
    }
}
