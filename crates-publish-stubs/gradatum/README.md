# gradatum

> Umbrella SDK facade — re-exports curated subsets of focused crates via Cargo features for downstream ergonomics.

**Status** : Alpha — placeholder \`v0.0.1\`. Source code private until \`v1.0\` public release. See [gradatum.org](https://gradatum.org) for project context.

**Memory backbone for AI agents — graduated.**

## Feature Flags

| Feature | Crates re-exported | Usage |
|---|---|---|
| `core` | `gradatum-core` | Shared primitives (always available) |
| `client` | `gradatum-sdk-rs` | Rust SDK for HTTP API integration |

## Public API

```rust
// Version constant
pub const VERSION: &str = "...";

// Re-exports (feature-gated)
#[cfg(feature = "core")]
pub use gradatum_core as core;

#[cfg(feature = "client")]
pub use gradatum_sdk_rs as sdk;
```

## Usage

```rust
[dependencies]
gradatum = { version = "0.0.1", features = ["core"] }
```

```rust
use gradatum::core::error::GradatumError;
```

## Crates in the Gradatum ecosystem

| Crate | Role |
|---|---|
| [`gradatum-core`](https://crates.io/crates/gradatum-core) | Shared primitives: errors, IDs, types |
| [`gradatum-markdown`](https://crates.io/crates/gradatum-markdown) | Parse/serialize MD + frontmatter + wikilinks |
| [`gradatum-index`](https://crates.io/crates/gradatum-index) | SQLite + FTS5 index layer |
| [`gradatum-storage`](https://crates.io/crates/gradatum-storage) | Storage trait + OpenDAL backends |
| [`gradatum-vault`](https://crates.io/crates/gradatum-vault) | Multi-vault registry + lifecycle |
| [`gradatum-cache`](https://crates.io/crates/gradatum-cache) | Moka LRU in-process cache |
| [`gradatum-embed`](https://crates.io/crates/gradatum-embed) | Embedder trait + HTTP/CPU backends |
| [`gradatum-chat`](https://crates.io/crates/gradatum-chat) | Chat trait + LLM backends + circuit breaker |
| [`gradatum-curator`](https://crates.io/crates/gradatum-curator) | LLM-powered note curation workflow |
| [`gradatum-search`](https://crates.io/crates/gradatum-search) | BM25 + semantic + RRF fusion search |
| [`gradatum-auth`](https://crates.io/crates/gradatum-auth) | JWT (Ed25519) + OIDC + API-key |
| [`gradatum-acl-policy`](https://crates.io/crates/gradatum-acl-policy) | ACL policy engine — deny-wins |
| [`gradatum-acl-auth`](https://crates.io/crates/gradatum-acl-auth) | Bearer verification + scope enforcement |
| [`gradatum-queue`](https://crates.io/crates/gradatum-queue) | SQLite-backed jobs queue with atomic leases |
| [`gradatum-engine`](https://crates.io/crates/gradatum-engine) | On-device inference (candle / llama.cpp) |
| [`gradatum-server`](https://crates.io/crates/gradatum-server) | HTTP/MCP facade :19090 |
| [`gradatum-worker`](https://crates.io/crates/gradatum-worker) | Async queue consumer |
| [`gradatum-admin`](https://crates.io/crates/gradatum-admin) | CLI ops: init/migrate/backup/restore |
| [`gradatum-cli`](https://crates.io/crates/gradatum-cli) | End-user CLI: read/write/search |
| [`gradatum-mcp-stub`](https://crates.io/crates/gradatum-mcp-stub) | MCP stdio → HTTP proxy |
| [`gradatum-sdk-rs`](https://crates.io/crates/gradatum-sdk-rs) | Rust SDK for HTTP API integration |

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0 (early access via maintainer)
- Roadmap : Phase 2.0b → `v0.1.0-alpha.3` → `v0.1.0-beta` → `v0.1.0` public
- License : Apache-2.0