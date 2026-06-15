# gradatum

> Umbrella SDK facade — re-exports curated subsets of focused crates via Cargo feature flags.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum` is the top-level entry point for downstream consumers. It re-exports focused
sub-crates through Cargo feature gates, letting you pull in only what you need.
The core runtime is a structured memory store for AI agents: write Markdown notes, curate
them via LLM, embed them for semantic search, and query them over HTTP or MCP.

All public traits carry `#[stability::unstable]` — no API stability guarantee before v1.0.

## Feature Flags

| Feature | Re-exported crate | Description |
|---|---|---|
| `core` | `gradatum-core` | Shared primitives (always useful as a baseline) |
| `client` | `gradatum-sdk-rs` | Rust HTTP client for the gradatum-server API |

## Usage

```toml
[dependencies]
gradatum = { version = "0.4.0", features = ["core"] }
```

```rust
use gradatum::core::error::GradatumError;
use gradatum::VERSION;
```

## Crate ecosystem

| Crate | Role |
|---|---|
| [`gradatum-core`](https://crates.io/crates/gradatum-core) | Shared primitives, traits, errors (L0) |
| [`gradatum-dto`](https://crates.io/crates/gradatum-dto) | Wire DTOs for HTTP API (L0) |
| [`gradatum-markdown`](https://crates.io/crates/gradatum-markdown) | Markdown + YAML frontmatter parser/writer |
| [`gradatum-storage`](https://crates.io/crates/gradatum-storage) | Storage trait + OpenDAL filesystem backend |
| [`gradatum-index`](https://crates.io/crates/gradatum-index) | SQLite + FTS5 index layer |
| [`gradatum-queue`](https://crates.io/crates/gradatum-queue) | SQLite-backed durable job queue |
| [`gradatum-embed`](https://crates.io/crates/gradatum-embed) | `Embedder` trait + HTTP and CPU backends |
| [`gradatum-chat`](https://crates.io/crates/gradatum-chat) | `Chat` trait + LLM backends (OpenAI-compat, heuristic) |
| [`gradatum-curator`](https://crates.io/crates/gradatum-curator) | LLM-powered note curation pipeline |
| [`gradatum-search`](https://crates.io/crates/gradatum-search) | BM25 + semantic search + RRF fusion |
| [`gradatum-vault`](https://crates.io/crates/gradatum-vault) | Vault lifecycle, write pipeline, drift detection |
| [`gradatum-cache`](https://crates.io/crates/gradatum-cache) | Moka LRU in-process cache |
| [`gradatum-auth`](https://crates.io/crates/gradatum-auth) | JWT Ed25519 + API key authentication |
| [`gradatum-acl-auth`](https://crates.io/crates/gradatum-acl-auth) | Bearer credential verification + scope enforcement |
| [`gradatum-acl-policy`](https://crates.io/crates/gradatum-acl-policy) | ACL policy engine (globset, deny-wins) |
| [`gradatum-db-sqlite`](https://crates.io/crates/gradatum-db-sqlite) | SQLite implementations of core storage traits |
| [`gradatum-engine`](https://crates.io/crates/gradatum-engine) | llama-server supervisor + transparent reverse proxy |
| [`gradatum-gateway`](https://crates.io/crates/gradatum-gateway) | Unified LLM router (aliases, circuit-breaker, fallback) |
| [`gradatum-warden`](https://crates.io/crates/gradatum-warden) | L0 network guard: IP filter + rate limit |
| [`gradatum-server`](https://crates.io/crates/gradatum-server) | HTTP/MCP server daemon (port 19090) |
| [`gradatum-worker`](https://crates.io/crates/gradatum-worker) | Async queue consumer (Apalis-backed) |
| [`gradatum-admin`](https://crates.io/crates/gradatum-admin) | Operator CLI: init, migrate, backfill, api-key |
| [`gradatum-cli`](https://crates.io/crates/gradatum-cli) | End-user CLI: write, search, read |
| [`gradatum-mcp-stub`](https://crates.io/crates/gradatum-mcp-stub) | MCP stdio adapter forwarding tool calls to the HTTP API |
| [`gradatum-sdk-rs`](https://crates.io/crates/gradatum-sdk-rs) | Rust SDK client for the HTTP API |

## License

Apache-2.0
