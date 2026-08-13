# gradatum-queue

> SQLite-backed durable job queue with atomic lease acquisition and automatic recovery.

**Status**: v2.0.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-queue` provides a persistent job queue backed by SQLite. It enforces two critical
guarantees that are required for correct concurrent processing:

- **Atomic claim** — `UPDATE…RETURNING` in a `BEGIN IMMEDIATE` transaction ensures only one
  consumer receives a given job, even under concurrent workers.
- **Lease recovery** — a job whose lease expires automatically becomes claimable again;
  `attempts` is incremented on each re-claim.

The crate provides two APIs:

- **`GradatumQueue`** — primary API (since 0.2.0). Implements the `QueueStore` trait from
  `gradatum-core`, backed by `SqliteQueueStore` in WAL mode with `QueueEvent` broadcast.
- **`SqliteQueue` / `Queue`** — deprecated since 0.2.0; scheduled for removal. Available
  for backward compatibility.
- **`LegacyQueue`** — synchronous `rusqlite`-based implementation, preserved for backward compatibility.

## Usage

```toml
[dependencies]
gradatum-queue = "2.0.0"
```

```rust
use gradatum_queue::GradatumQueue;
use gradatum_core::QueueStore;

// GradatumQueue wraps a SqliteQueueStore (see the gradatum workspace).
let queue = GradatumQueue::new(store);

// Enqueue a job. `job` is a `gradatum_core::JobRecord` — assembled from its five
// blocks (spec, scheduling, lifecycle, retry, lineage); see `gradatum_core::job`.
let id: ulid::Ulid = queue.enqueue(job).await?;

// Atomic lease acquisition — only one consumer receives a given job.
// `tenant_filter`: `None` = no tenant clause (single-tenant / backward-compatible).
if let Some(leased) = queue.dequeue(None).await? {
    // process job …
}
```

## License

Apache-2.0
