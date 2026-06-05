# gradatum-mcp

> Full MCP server implementation for gradatum (Phase 2.x+). See [`gradatum-mcp-stub`](https://crates.io/crates/gradatum-mcp-stub) for the current stdio proxy.

**Status** : Alpha — placeholder `v0.0.1`. Source code private until `v1.0` public release. See [gradatum.org](https://gradatum.org) for project context.

**Part of [`gradatum`](https://crates.io/crates/gradatum)** — Memory backbone for AI agents.

## Planned scope (Phase 2.x+)

`gradatum-mcp` will implement a full MCP server (not a proxy):

- Native MCP Streamable HTTP transport (MCP 2025-03-26 spec)
- Direct in-process connection to storage/index layers
- Extended tool set beyond the 10 tools in `gradatum-mcp-stub`
- Sampling and resource endpoints
- MCP authorization integration

## Current alternative

Use [`gradatum-mcp-stub`](https://crates.io/crates/gradatum-mcp-stub) — stdio proxy that forwards to `gradatum-server` HTTP API.

## Documentation

- Project : <https://gradatum.org>
- Source : private until v1.0
- Roadmap : Phase 2.x+ (post-Phase 2.1)
- License : Apache-2.0