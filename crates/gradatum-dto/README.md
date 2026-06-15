# gradatum-dto

> Wire DTOs for the gradatum HTTP API — single source of truth for all `Vault*Request` structs.

**Status**: Alpha (v0.4.x) — public, Apache-2.0. API not yet stable before v1.0.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-dto` defines the shared request and response types consumed by three crates:

- `gradatum-server` — deserializes incoming HTTP `/api/v1/*` requests
- `gradatum-mcp-stub` — generates MCP `inputSchema` JSON (via the `schemars` feature)
- `gradatum-sdk-rs` — typed Rust client bindings

This crate sits at DAG level L0: it has zero workspace dependencies. Types use flat `String`
fields (no domain types like `TenantId` or `NoteId`) to keep the wire contract stable and
independent of internal domain evolution.

## Usage

```toml
# Without JSON Schema generation (server / SDK):
gradatum-dto = "0.4.0"

# With JSON Schema generation (MCP stub):
gradatum-dto = { version = "0.4.0", features = ["schemars"] }
```

```rust
use gradatum_dto::{VaultWriteRequest, VaultSearchRequest, VaultReadRequest};
```

## Features

| Feature | Description |
|---|---|
| `schemars` | Derives `JsonSchema` on all request structs (used by `gradatum-mcp-stub`) |

## License

Apache-2.0
