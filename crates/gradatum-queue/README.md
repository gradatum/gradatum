# gradatum-queue

> SQLite-backed durable job queue with atomic lease acquisition and automatic recovery.

**Status**: 0.x — API not yet stable. Apache-2.0.
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
gradatum-queue = "0.7.6"
```

```rust
use gradatum_queue::GradatumQueue;
use gradatum_core::QueueStore;

// GradatumQueue wraps a SqliteQueueStore (see the gradatum workspace).
let queue = GradatumQueue::new(store);

// Enqueue a job.
let id: ulid::Ulid = queue.enqueue(job).await?;

// Atomic lease acquisition — only one consumer receives a given job.
if let Some(leased) = queue.dequeue().await? {
    // process job …
}
```

## License

Apache-2.0
