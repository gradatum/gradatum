# gradatum-server

> HTTP/MCP server daemon (port 19090) — stateless API facade for vault reads, writes, and search.

**Status**: Alpha (v0.7.6) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-server` is the main daemon. It exposes an HTTP REST API and an MCP endpoint,
handles read and search requests synchronously, and enqueues write operations for
asynchronous processing by `gradatum-worker`.

Key properties:

- Stateless HTTP facade — no business logic, delegates to the domain layer.
- Write path: enqueues note to SQLite job queue, returns immediately (202 Accepted).
- Read/search path: queries `gradatum-index` and `gradatum-search` directly.
- JWT authentication via `gradatum-auth` (Ed25519 tokens, audience-scoped).
- Rate limiting and IP filtering via `gradatum-warden`.
- MCP transport: Streamable HTTP (2025-03-26 spec) on `/mcp`, SSE on `/sse`.
- Prometheus metrics on `/metrics` (loopback-only by default).
- Curated metrics timeseries: `curated_metrics` module (`collect_curated_samples`) samples ~60 curated
  Prometheus series every 60 s into `metric_sample` (via `IndexStore`), with 14-day retention and
  server-side downsampling (`compute_bucket_ms`). The `metric-sample` task is the 8th entry in
  `ALL_SCHEDULED_TASKS` and appears in `/api/v1/system/scheduled`.

## Usage

```bash
gradatum-server --config /etc/gradatum/server.toml
```

## HTTP Endpoints (non-exhaustive)

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/health` | None | Health check — `{"status":"ok","version":"..."}` |
| `POST` | `/auth/exchange` | API key | Exchange API key for a JWT |
| `POST` | `/api/v1/vault_write` | Bearer | Write a note (enqueued, returns ULID) |
| `POST` | `/api/v1/vault_search` | Bearer | Hybrid BM25 + semantic search (optional `from_ms`/`to_ms` temporal filter) |
| `POST` | `/api/v1/vault_read` | Bearer | Read note by path |
| `POST` | `/api/v1/vault_list` | Bearer | List notes with filters and pagination |
| `GET` | `/api/v1/vault_status` | Bearer | Vault statistics |
| `GET` | `/api/v1/vault_authors` | Bearer | List note authors |
| `GET` | `/api/v1/vault_tags` | Bearer | List tags with frequencies |
| `POST` | `/api/v1/vault_graph` | Bearer | Wikilink graph from a root note |
| `POST` | `/api/v1/vault_links` | Bearer | Wikilinks for a note |
| `POST` | `/api/v1/vault_trace` | Bearer | Trace chain through a note |
| `POST` | `/api/v1/vault_context` | Bearer | Context assembly — RRF retrieval + composite scoring + budget-aware selection + structured markdown output (`mode=Assembled`\|`Raw`; optional `reference_mode`, `session_id`) |
| `GET` | `/api/v1/lessons/recall` | Bearer | BM25 lesson recall (optional `rank` and `semantic` params) |
| `POST` | `/api/v1/proactive_recall` | Bearer | Pull proactive or contextual memory surface |
| `POST` | `/api/v1/proactive_recall/feedback` | Bearer | Record which surfaced notes were accepted |
| `GET` | `/api/v1/system/scheduled` | Bearer | Health snapshot for all 8 recurring scheduled tasks |
| `GET` | `/api/v1/system/metrics/catalog` | Bearer | Static catalog of ~60 curated Prometheus series (`{ key, group, kind, unit, instrumented }`) — no DB query |
| `GET` | `/api/v1/system/metrics/timeseries` | Bearer | Downsampled timeseries for selected curated series (`series` CSV, `from_ms`/`to_ms`, `max_points`; server-side bucket AVG via `compute_bucket_ms`) |
| `GET` | `/api/v1/system/traces` | Bearer | Paginated read of `session_trace` records |
| `GET` | `/api/v1/notes/by-status` | Bearer | Paginated note listing by status (metadata only; includes `downgraded` notes excluded from search) |

## License

Apache-2.0
