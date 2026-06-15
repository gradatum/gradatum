# gradatum-queue

> SQLite-backed durable job queue with atomic lease acquisition and automatic recovery.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-queue` provides a persistent job queue backed by SQLite. It enforces two critical
guarantees that are required for correct concurrent processing:

- **Atomic claim** — `UPDATE…RETURNING` in a `BEGIN IMMEDIATE` transaction ensures only one
  consumer receives a given job, even under concurrent workers.
- **Lease recovery** — a job whose lease expires automatically becomes claimable again;
  `attempts` is incremented on each re-claim.

The crate provides two APIs:

- `SqliteQueue` / `Queue` trait — async `sqlx`-based implementation (WAL mode). Current default.
- `LegacyQueue` — synchronous `rusqlite`-based implementation, preserved for Phase 1 test compatibility.

## Usage

```toml
[dependencies]
gradatum-queue = "0.4.3"
```

```rust
use gradatum_queue::{SqliteQueue, Queue, NewJob};
use std::path::Path;
use std::time::Duration;

// Open the queue (creates tables if needed).
let queue = SqliteQueue::new(Path::new("/var/lib/gradatum/db/queue.sqlite")).await?;

// Enqueue a new job.
let new_job = NewJob {
    kind: "Curate".to_string(),
    payload: serde_json::to_string(&spec)?,
};
let job_id = queue.enqueue(new_job).await?;

// In the worker — atomic lease acquisition.
if let Some(leased) = queue.lease(&["Curate", "Embed"], Duration::from_secs(300)).await? {
    // process job …
    queue.complete(leased.id).await?;
    // or on failure:
    // queue.fail(leased.id, "reason").await?;
}
```

## License

Apache-2.0
