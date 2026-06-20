# gradatum-worker

> Async queue consumer — processes curator and embedding jobs from the SQLite Apalis queue.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-worker` is the async background processor that drains the job queue populated
by `gradatum-server`. It uses Apalis as the job dispatch framework backed by SQLite.

Job types processed in v0.4.x:

| Job type (`Job::` variant) | Handler | Description |
|---|---|---|
| `Curate` | `handle_curate` | Runs the `CuratorPipeline` on a queued note (section routing, tags, wikilinks, dedup) |
| `Embed` | `handle_embed` | Computes and stores embeddings for a note via the configured `Embedder` backend |
| `ReIndex` | `handle_reindex` | Full index rebuild for a vault — **not yet implemented** (returns a typed error in v0.4.x) |
| `Forget` | `handle_forget` | Semantic forget — marks a batch of notes as forgotten (frontmatter + index update) |
| `Purge` | `handle_purge` | Lifecycle purge — permanently deletes Garbage notes after the grace period |

The worker uses `BEGIN IMMEDIATE` transactions for atomic job dequeue — preventing the
read-then-write deadlock that occurs under concurrency with `BEGIN DEFERRED`.

## Usage

```bash
gradatum-worker --config /etc/gradatum/server.toml
```

## Configuration (TOML, shared with gradatum-server)

```toml
[worker]
concurrency = 4
lease_timeout_ms = 300000
```

## License

Apache-2.0
