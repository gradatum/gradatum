# gradatum-db-sqlite

> Async SQLite queue store (`SqliteQueueStore`) — `sqlx`-backed job queue for gradatum-worker.

**Status**: Alpha (v0.7.6) — internal (publish = false). API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-db-sqlite` provides the concrete SQLite-backed `SqliteQueueStore` used by
`gradatum-worker` and `gradatum-server` as the async job queue backend.

The only public type is **`SqliteQueueStore`**: async SQLite queue via `sqlx` (WAL mode,
`UPDATE…RETURNING` atomic claim, broadcast notifications for zero-poll consumers).

> Note: the full-text index and Markdown document store live in `gradatum-index` and
> `gradatum-storage` respectively — not in this crate.

## Usage

```toml
[dependencies]
gradatum-db-sqlite = { path = "../gradatum-db-sqlite", version = "0.7.6" }
```

```rust
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
use sqlx::SqlitePool;

let pool = SqlitePool::connect("sqlite:///var/lib/gradatum/queue.db?mode=rwc").await?;
run_migrations(&pool).await?;
let store = SqliteQueueStore::new(pool);
```

## Architecture

```text
gradatum-core (L0) — QueueStore trait
    ↑
gradatum-db-sqlite (L2) — SqliteQueueStore (this crate, internal only)
    ↑
gradatum-worker / gradatum-server (L4)
```

## License

Apache-2.0
