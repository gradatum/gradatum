# gradatum-worker

> Async queue consumer for curator LLM processing and maintenance jobs.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Usage (Phase 2.0b)

```
gradatum-worker [--config <path>]
```

## Job types processed

| Job type | Description |
|---|---|
| `curate_note` | LLM curator decision for a queued note |
| `drift_check` | Periodic drift detection (Phase A scan) |
| `index_rebuild` | Full index rebuild for a vault |
| `purge_expired_revocations` | Clean up expired JWT revocations |

## Configuration (TOML)

```toml
data_root = "/var/lib/gradatum"
queue_path = "/var/lib/gradatum/queue.db"
worker_concurrency = 4           # parallel job consumers
lease_timeout_ms = 300000        # 5 minutes per job
llm_endpoint = "http://127.0.0.1:8080"   # OpenAI-compat endpoint
```

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0 (Phase 2.0b implementation)
- Roadmap : Phase 2.0b → `v0.1.0` public
- License : Apache-2.0