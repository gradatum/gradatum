# gradatum-worker

> Async queue consumer — processes curator and embedding jobs from the SQLite Apalis queue.

**Status**: v2.0.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-worker` is the async background processor that drains the job queue populated
by `gradatum-server`. It uses Apalis as the job dispatch framework backed by SQLite.

Job types:

| Job type (`Job::` variant) | Handler | Description |
|---|---|---|
| `Curate` | `handle_curate` | Runs the `CuratorPipeline` on a queued note (section routing, tags, wikilinks, dedup) |
| `Embed` | `handle_embed` | Computes and stores embeddings for a note via the configured `Embedder` backend |
| `ReIndex` | `handle_reindex` | Full index rebuild for a vault — **not yet implemented** (handler returns an error in the current release) |
| `Forget` | `handle_forget` | Semantic forget — marks a batch of notes as forgotten (frontmatter + index update) |
| `Purge` | `handle_purge` | Lifecycle purge — permanently deletes Garbage notes after the grace period |
| `Validate` | `handle_validate` | Deterministic quality gate — scores each distilled synthesis; degrades trust + tags `quality-low` on score < 0.75 |

The worker uses `BEGIN IMMEDIATE` transactions for atomic job dequeue — preventing the
read-then-write deadlock that occurs under concurrency with `BEGIN DEFERRED`.

### `Validate` worker

The `Validate` worker is the gate between distillation and persistence. `handle_distill`
enqueues `Job::Validate` instead of persisting directly; `handle_validate` scores, decides
disposition, then persists.

**Quality score** — composite formula, all factors in `[0.0, 1.0]`:

| Factor | Source |
|---|---|
| `grounding` | Cosine similarity: synthesis embedding ↔ centroid of source embeddings |
| `f17` | Source recency — exponential-decay on source `anchor_ms` |
| `f47` | Mean trust across source notes |
| `num_penalty` | Numeric-coherence — orphan numbers in synthesis: −0.15 each (floor 0.5) |
| `entity_penalty` | Orphan-entity — uppercase tokens absent from all sources: −0.10 each (floor 0.5) |

**score** = `grounding × f17 × f47 × num_penalty × entity_penalty` (clamped to `[0.0, 1.0]`)

**Disposition** (default threshold: **0.75**):

| Outcome | Condition | Effect |
|---|---|---|
| `accept` | `score ≥ 0.75` | Stored with `base_trust`; no quality tag |
| `degrade` | `score < 0.75` | Stored with `trust = base_trust × score`; `quality-low` tag added |

Scoring errors fall back to a neutral pass (score = 1.0) — no synthesis note is ever
permanently lost due to an embedder failure.

This worker is **deterministic** (heuristics + embedder, no LLM). Automated healing is
deferred to a future feature.

## Usage

```bash
gradatum-worker --config /etc/gradatum/server.toml
```

## Configuration (TOML, shared with gradatum-server)

Per-job-kind worker settings live under `[apalis.workers.<kind>]`:

```toml
[apalis.workers.curate]
concurrency = 4      # default 2
timeout_secs = 300   # default 30
max_retries = 3      # default 3
```

## License

Apache-2.0
