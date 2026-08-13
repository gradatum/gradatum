# gradatum-sdk-rs

> Rust SDK client for the gradatum-server HTTP API.

**Status**: v2.0.0 — public, Apache-2.0. **Placeholder — no client surface is implemented yet.**
The crate compiles and exports only a `VERSION` constant; there is no public API to be stable
about. A typed async client is planned for a future release.
Part of **[gradatum](https://crates.io/crates/gradatum)** — memory backbone for AI agents. · [github](https://github.com/gradatum/gradatum) · [gradatum.org](https://gradatum.org)

## Overview

`gradatum-sdk-rs` is a placeholder crate reserved for a future typed async Rust client
for the `gradatum-server` HTTP API. No client type is implemented yet.

The crate compiles cleanly and exports only a `VERSION` constant. Full implementation
is planned for a future release.

## Usage

```toml
[dependencies]
gradatum-sdk-rs = "2.0.0"
```

No public types are implemented yet. Use direct HTTP calls to `gradatum-server`
(`/api/v1/...`), or an MCP client pointed at its native `/mcp` endpoint, in the meantime.

## License

Apache-2.0
