# gradatum-db-sqlite

> Async SQLite queue store (`SqliteQueueStore`) — `rusqlite`-backed job queue for gradatum-worker.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-db-sqlite` provides the concrete SQLite-backed `SqliteQueueStore` used by
`gradatum-worker` and `gradatum-server` as the async job queue backend.

The two central public types are **`SqliteQueueStore`** and **`QueueDb`**: an async SQLite
queue over `rusqlite` (WAL mode, `UPDATE…RETURNING` atomic claim, broadcast notifications for
zero-poll consumers). `QueueDb` is the shared connection handle; `SqliteQueueStore` is built
from it. The crate also exports `open_queue_db`, `open_queue_db_existing`,
`open_queue_db_in_memory`, `run_migrations`, `apply_sqlite_pragmas` and the `idempotency_*`
helpers.

> Note: the full-text index and Markdown document store live in `gradatum-index` and
> `gradatum-storage` respectively — not in this crate.

## Usage

```toml
[dependencies]
gradatum-db-sqlite = "2.1.0"
```

```rust
use gradatum_db_sqlite::{SqliteQueueStore, open_queue_db, run_migrations};
use std::path::Path;

// Creates the file if absent, and applies WAL + a 5 s busy_timeout.
let db = open_queue_db(Path::new("/var/lib/gradatum/queue.db")).await?;

// Returns the number of migrations applied — 0 on an already up-to-date database.
// Failures surface as `gradatum_core::job::QueueError`.
let applied = run_migrations(&db).await?;
println!("{applied} migration(s) applied");

let store = SqliteQueueStore::new(db);
```

## Architecture

```text
gradatum-core (L0) — QueueStore trait
    ↑
gradatum-db-sqlite (L2) — SqliteQueueStore (this crate)
    ↑
gradatum-worker / gradatum-server (L4)
```

## License

Apache-2.0
