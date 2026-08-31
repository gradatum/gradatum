# gradatum-index

> SQLite + FTS5 index layer — implements `DocumentStore`, `IndexStore`, and `VectorStore` traits with three-level drift detection.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-index` provides `SqliteIndex`, the primary storage implementation for gradatum.
It combines full-text search via SQLite FTS5 with dense vector storage for semantic search,
and exposes the `Index` blanket trait through implementations of all three `gradatum-core`
storage traits.

Key features:

- **FTS5 full-text search** with BM25 ranking over note bodies and metadata.
- **Cosine similarity** over vectors stored in SQLite (no external vector database); exhaustive
  by default, with in-SQLite ANN available behind the `sqlite-vec-ann` feature.
- **Four mandatory PRAGMAs** on every connection open: `WAL`, `synchronous=NORMAL`,
  `busy_timeout=5000`, `foreign_keys=ON`.
- **Schema migrations** — applied automatically from embedded SQL migration files.
- **Three-level drift detection** via `drift::scan_phase_a`:
  - Level 1: file size check (fast, no read).
  - Level 2: first 4 KB prefix hash.
  - Level 3: full SHA-256 (only when Level 2 mismatches).
- **Temporal index** (`temporal_index` table, migration 0013) — per-note `anchor_ms` and
  `doc_kind`. Used by `vault_search` temporal range filter (`from_ms` / `to_ms`) and by the
  `recency_factor` composite scoring signal. `anchor_ms` is populated from an explicit
  `occurred_at` write field (when provided) or derived from `created_at` as fallback.
- **Scheduled task health** (`scheduled_task_health` + `scheduled_task_error` tables,
  migration 0026) — observability for recurring in-process tasks:
  - `record_task_run(task_name, outcome, duration_ms, error, now_ms)` — upserts the health snapshot
    (increments `run_count`), appends to the error log on failure, triggers a lazy 7-day
    purge. Never panics.
  - `seed_scheduled_task(task_name)` — idempotent boot-time registration.
  - `list_scheduled_health(now_ms)` — returns all registered tasks with `errors_24h` count.
- **Curated metrics timeseries** (`metric_sample` table, migration 0027) — persistent timeseries
  store for Prometheus-scraped curated metrics:
  - Table: `metric_sample (series TEXT, ts_ms INTEGER, value REAL, PRIMARY KEY (series, ts_ms)) WITHOUT ROWID` + `INDEX idx_metric_sample_ts ON metric_sample(ts_ms)`.
  - `insert_metric_samples(ts_ms, samples: &[(String, f64)])` — batch INSERT OR IGNORE (PK prevents tick collisions).
  - `query_metric_timeseries(series, from_ms, to_ms, bucket_ms)` — range query with server-side downsample: `AVG(value) GROUP BY (ts_ms / bucket_ms)`, inclusive bounds, each point timestamped with the `MIN(ts_ms)` of its bucket. With the minimum `bucket_ms` of 60_000 and a one-minute sampling period, each bucket holds a single sample, so the result is the raw series. Returns `Vec<MetricSamplePoint>` from `gradatum-core`.
  - `purge_metric_samples(cutoff_ms)` — `DELETE WHERE ts_ms < cutoff_ms` (lazy 14-day retention).
  - `list_distinct_metric_series()` — distinct `series` values present in the table.

## Usage

```toml
[dependencies]
gradatum-index = "2.1.0"
```

```rust
use gradatum_index::SqliteIndex;

let index = SqliteIndex::open(Path::new("/var/lib/gradatum/index.db")).await?;
// or for tests:
let index = SqliteIndex::open_in_memory().await?;
```

## Feature Flags

| Feature | Default | Description |
|---|---|---|
| `sqlite-vec-ann` | no | Enables ANN (approximate nearest neighbor) search via the `sqlite-vec` `vec0` virtual table. Runtime registration of the extension (`sqlite3_auto_extension`) is the responsibility of the binary crate. |

## License

Apache-2.0
