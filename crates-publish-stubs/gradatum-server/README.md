# gradatum-server

> Stateless HTTP/MCP facade on port 19090. Handles read/search requests and enqueues write operations.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Usage

```
gradatum-server [--config <path>]
```

## HTTP Endpoints

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/health` | None | Health check — returns `{"status":"ok","version":"..."}` |
| `GET` | `/metrics` | Loopback only | Prometheus metrics (port :19091) |
| `POST` | `/api/v1/vault_search` | Bearer | Full-text + semantic search |
| `POST` | `/api/v1/vault_read` | Bearer | Read note by path |
| `POST` | `/api/v1/vault_list` | Bearer | List notes with pagination |
| `GET` | `/api/v1/vault_status` | Bearer | Vault status and stats |
| `GET` | `/api/v1/vault_authors` | Bearer | List note authors |
| `GET` | `/api/v1/vault_tags` | Bearer | List tags with frequencies |
| `POST` | `/api/v1/vault_graph` | Bearer | Wikilink graph from a root note |
| `POST` | `/api/v1/vault_links` | Bearer | Wikilinks for a note |
| `POST` | `/api/v1/vault_trace` | Bearer | Trace chain through a note |
| `POST` | `/api/v1/vault_context` | Bearer | Context window for a note |

## MCP Endpoint

| Path | Description |
|---|---|
| `/mcp` | Streamable HTTP (MCP 2025-03-26) |
| `/sse` | SSE legacy transport |

## Configuration (TOML)

```toml
bind = "127.0.0.1:19090"     # C3: TLS required for non-loopback
data_root = "/var/lib/gradatum"
jwt_public_key_path = "/etc/gradatum/jwt_ed25519.pub"
```

## Auth

JWT Ed25519 bearer token. Audience-scoped (`read` / `write` / `admin`).
Generate via: `gradatum-admin init --preset hierarchical --root /var/lib/gradatum`

## Graceful shutdown

SIGTERM → 30-second drain.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.0a Foundation + Read API → `v0.1.0` public
- License : Apache-2.0