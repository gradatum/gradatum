# gradatum-dto

> Wire DTOs for the gradatum HTTP API — single source of truth for all `Vault*Request` structs.

**Status**: v2.1.0 — public, Apache-2.0. Stable API under SemVer.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-dto` defines the shared request and response types consumed across the gradatum
stack (`gradatum-server`, `gradatum-worker`, `gradatum-admin`, `gradatum-engine`). Chiefly:

- `gradatum-server` — deserializes incoming HTTP `/api/v1/*` requests **and** generates the
  MCP `inputSchema` JSON for its native `/mcp` endpoint (via the `schemars` feature)

`gradatum-sdk-rs` does **not** depend on this crate: it is a placeholder with no client surface.

This crate sits at DAG level L1: it depends on `gradatum-core` for the shared scope types.
Request structs carry domain types from `gradatum_core::scope` — chiefly `TenantId` (the
dominant field, present on nearly every request) and `VaultId` — rather than bare strings,
so tenant and vault identifiers stay typed end-to-end across the server and the MCP surface.

> `gradatum-mcp-stub`, a separate stdio→HTTP adapter that used to be a second consumer of the
> `schemars` feature, is retired as of `2.0.0` (`publish = false`, source kept in-tree). MCP
> clients now connect directly to `gradatum-server`'s native `/mcp` endpoint, which is the
> sole `schemars` consumer today.

## Usage

```toml
# Without JSON Schema generation (SDK, most consumers):
gradatum-dto = "2.1.0"

# With JSON Schema generation (native MCP surface, gradatum-server):
gradatum-dto = { version = "2.1.0", features = ["schemars"] }
```

```rust
use gradatum_dto::{VaultWriteRequest, VaultSearchRequest, VaultReadRequest};
```

## Features

| Feature | Description |
|---|---|
| `schemars` | Derives `JsonSchema` on all request structs (used by `gradatum-server`'s native `/mcp` endpoint) |

## License

Apache-2.0
