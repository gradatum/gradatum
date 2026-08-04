# gradatum-dto

> Wire DTOs for the gradatum HTTP API — single source of truth for all `Vault*Request` structs.

**Status**: v1.0.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-dto` defines the shared request and response types consumed across the gradatum
stack (`gradatum-server`, `gradatum-worker`, `gradatum-admin`, `gradatum-engine`,
`gradatum-mcp-stub`). Chiefly:

- `gradatum-server` — deserializes incoming HTTP `/api/v1/*` requests
- `gradatum-mcp-stub` — generates MCP `inputSchema` JSON (via the `schemars` feature)

`gradatum-sdk-rs` does **not** depend on this crate: it is a placeholder with no client surface
in `1.0.0`.

This crate sits at DAG level L1: it depends on `gradatum-core` for the shared scope types.
Request structs carry domain types from `gradatum_core::scope` — chiefly `TenantId` (the
dominant field, present on nearly every request) and `VaultId` — rather than bare strings,
so tenant and vault identifiers stay typed end-to-end across the server and the MCP stub.

## Usage

```toml
# Without JSON Schema generation (server / SDK):
gradatum-dto = "1.0.0"

# With JSON Schema generation (MCP stub):
gradatum-dto = { version = "1.0.0", features = ["schemars"] }
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
