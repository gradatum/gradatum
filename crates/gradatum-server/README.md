# gradatum-server

> HTTP/MCP server daemon (port 19090) — stateless API facade for vault reads, writes, and search.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
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

## Usage

```bash
gradatum-server --config /etc/gradatum/server.toml
```

## HTTP Endpoints

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/health` | None | Health check — `{"status":"ok","version":"..."}` |
| `POST` | `/api/v1/vault_write` | Bearer | Write a note (enqueued, returns ULID) |
| `POST` | `/api/v1/vault_search` | Bearer | Hybrid BM25 + semantic search |
| `POST` | `/api/v1/vault_read` | Bearer | Read note by path |
| `POST` | `/api/v1/vault_list` | Bearer | List notes with filters and pagination |
| `GET` | `/api/v1/vault_status` | Bearer | Vault statistics |
| `GET` | `/api/v1/vault_authors` | Bearer | List note authors |
| `GET` | `/api/v1/vault_tags` | Bearer | List tags with frequencies |
| `POST` | `/api/v1/vault_graph` | Bearer | Wikilink graph from a root note |
| `POST` | `/api/v1/vault_links` | Bearer | Wikilinks for a note |
| `POST` | `/api/v1/vault_trace` | Bearer | Trace chain through a note |
| `POST` | `/api/v1/vault_context` | Bearer | Context window for a note |
| `POST` | `/auth/exchange` | API key | Exchange API key for a JWT |

## License

Apache-2.0
